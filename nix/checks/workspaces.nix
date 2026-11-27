{
  pkgs,
  flake,
  inputs,
  ...
}:
let
  inherit (pkgs) lib;
  domain = "ws.test";
  # What a person's computer has to reach herdr's two runtimes.
  agents = inputs.llm-agents.packages.${pkgs.stdenv.hostPlatform.system};

  certs = pkgs.runCommand "workspaces-test-certs" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
      -subj "/CN=workspaces test CA" -keyout ca.key -out $out/ca.crt \
      -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj "/CN=${domain}" -keyout $out/server.key -out server.csr
    printf 'subjectAltName=DNS:${domain},DNS:*.${domain}\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > ext
    openssl x509 -req -in server.csr -CA $out/ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile ext -out $out/server.crt
  '';

  sshKey = pkgs.runCommand "workspaces-test-ssh-key" { nativeBuildInputs = [ pkgs.openssh ]; } ''
    mkdir $out
    ssh-keygen -q -t ed25519 -N "" -C test -f $out/id
  '';

  # Both have herdr's two runtimes, which people opt into, besides T3 Code.
  workspaces = {
    alice = {
      ip = "10.232.0.10";
      owner = "alice";
    };
    bob = {
      ip = "10.232.0.11";
      owner = "bob";
      os = "nixos";
    };
  };
  apps = [
    "t3code"
    "herdr"
    "herdrcore"
  ];
  clientIP = "192.168.1.3";
  users = [
    "alice"
    "bob"
    "carol"
  ];
  baseDN = "dc=ws,dc=test";

  # The directory, as in production: posix accounts, and groups listing
  # their members' uids. alice makes workspaces of her own; carol is an
  # admin; bob is in neither group.
  directory = ''
    dn: ${baseDN}
    objectClass: dcObject
    objectClass: organization
    dc: ws
    o: workspaces test

    dn: ou=users,${baseDN}
    objectClass: organizationalUnit
    ou: users

    dn: ou=groups,${baseDN}
    objectClass: organizationalUnit
    ou: groups
  ''
  + lib.concatImapStrings (i: user: ''

    dn: uid=${user},ou=users,${baseDN}
    objectClass: inetOrgPerson
    objectClass: posixAccount
    uid: ${user}
    cn: ${user}
    sn: ${user}
    mail: ${user}@${domain}
    uidNumber: ${toString (10500 + i)}
    gidNumber: 100
    homeDirectory: /home/${user}
    userPassword: password
  '') users
  + ''

    dn: cn=workbench,ou=groups,${baseDN}
    objectClass: posixGroup
    cn: workbench
    gidNumber: 20001
    memberUid: alice

    dn: cn=workbench-admins,ou=groups,${baseDN}
    objectClass: posixGroup
    cn: workbench-admins
    gidNumber: 20002
    memberUid: carol
  '';

  # The host's commands for workspaces made at run time: a credential for
  # an owner, unless the test takes it away, and whether the directory
  # still has them in the group.
  environmentCommand = pkgs.writeShellScript "workspace-env" ''
    test -e /run/no-credential && exit 1
    echo "WORKSPACE_TOKEN=token-of-$1"
  '';
  ownerCommand = pkgs.writeShellScript "workspace-owner" ''
    ${pkgs.openldap}/bin/ldapsearch -x -LLL -H ldap://127.0.0.1 -b ou=groups,${baseDN} \
      "(&(cn=workbench)(memberUid=$1))" dn | grep -q '^dn:'
  '';

  # Attaches to a workspace's console its node shares, the way `workbench
  # console` does, and presses Enter until the guest's login prompt shows.
  consoleLogin = pkgs.writeText "console-login.py" ''
    import socket, sys, time
    s = socket.socket(socket.AF_UNIX)
    s.connect("/run/workbench-node/console.sock")
    s.sendall(sys.argv[1].encode() + b"\n")
    s.settimeout(2)
    out = b""
    while b"login:" not in out:
        s.sendall(b"\r")
        try:
            out += s.recv(65536)
        except TimeoutError:
            pass
        time.sleep(0.5)
    assert out.startswith(b"OK\n"), out[:200]
  '';

  # Signs in through oauth2-proxy and Dex's password form, as a browser
  # does, and keeps the cookies for curl.
  ssoLogin = pkgs.writeText "sso-login.py" ''
    import http.cookiejar, re, sys, urllib.parse, urllib.request
    user, start = sys.argv[1], sys.argv[2]
    jar = http.cookiejar.MozillaCookieJar(f"/tmp/sso-{user}.jar")
    web = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    page = web.open(start)
    form = re.search(r'<form[^>]*action="([^"]+)"', page.read().decode())
    action = urllib.parse.urljoin(page.url, form.group(1).replace("&amp;", "&"))
    done = web.open(action, urllib.parse.urlencode({"login": user, "password": "password"}).encode())
    body = done.read().decode()
    # Dex may still ask to grant the client access.
    req = re.search(r'name="req" value="([^"]+)"', body)
    if 'name="approval"' in body and req:
        done = web.open(done.url, urllib.parse.urlencode({"req": req.group(1), "approval": "approve"}).encode())
        body = done.read().decode()
    jar.save(ignore_discard=True)
    print(done.url)
    sys.stdout.write(body)
  '';
in
pkgs.testers.runNixOSTest {
  name = "workspaces";

  nodes = {
    host = {
      imports = [ flake.nixosModules.default ];

      virtualisation = {
        memorySize = 10240;
        cores = 4;
        diskSize = 30720;
      };
      networking.firewall.allowedTCPPorts = [ 443 ];
      environment.systemPackages = [
        pkgs.openssh
        pkgs.jq
        pkgs.curl
        pkgs.python3
        pkgs.openldap
      ];
      security.pki.certificateFiles = [ "${certs}/ca.crt" ];
      networking.hosts."127.0.0.1" = [ domain ];

      services.openldap = {
        enable = true;
        urlList = [ "ldap:///" ];
        settings.children = {
          "cn=schema".includes = map (s: "${pkgs.openldap}/etc/schema/${s}.ldif") [
            "core"
            "cosine"
            "nis"
            "inetorgperson"
          ];
          "olcDatabase={1}mdb".attrs = {
            objectClass = [
              "olcDatabaseConfig"
              "olcMdbConfig"
            ];
            olcDatabase = "{1}mdb";
            olcDbDirectory = "/var/lib/openldap/db";
            olcSuffix = baseDN;
            olcRootDN = "cn=admin,${baseDN}";
            olcRootPW = "admin";
            olcAccess = [
              "{0}to attrs=userPassword by anonymous auth by * none"
              "{1}to * by * read"
            ];
          };
        };
        declarativeContents.${baseDN} = directory;
      };
      systemd.tmpfiles.rules = [ "d /var/lib/openldap/db 0700 openldap openldap -" ];

      # Dex stands in for Keycloak: it reads the users and their groups from
      # the directory, with the same claims, and again at each refresh.
      services.dex = {
        enable = true;
        settings = {
          issuer = "https://${domain}/dex";
          storage.type = "memory";
          web.http = "127.0.0.1:5556";
          oauth2.skipApprovalScreen = true;
          connectors = [
            {
              type = "ldap";
              id = "ldap";
              name = "LDAP";
              config = {
                host = "127.0.0.1:389";
                insecureNoSSL = true;
                userSearch = {
                  baseDN = "ou=users,${baseDN}";
                  filter = "(objectClass=posixAccount)";
                  username = "uid";
                  idAttr = "uid";
                  emailAttr = "mail";
                  nameAttr = "cn";
                  preferredUsernameAttr = "uid";
                };
                groupSearch = {
                  baseDN = "ou=groups,${baseDN}";
                  filter = "(objectClass=posixGroup)";
                  userMatchers = [
                    {
                      userAttr = "uid";
                      groupAttr = "memberUid";
                    }
                  ];
                  nameAttr = "cn";
                };
              };
            }
          ];
          staticClients = [
            {
              id = "workbench";
              name = "workbench";
              redirectURIs = [ "https://${domain}/oauth2/callback" ];
              secret = "test-client-secret";
            }
          ];
        };
      };
      systemd.services.dex = {
        after = [ "openldap.service" ];
        wants = [ "openldap.service" ];
      };
      services.nginx.virtualHosts.${domain}.locations."/dex/".proxyPass = "http://127.0.0.1:5556";

      # A host on its own: the front, and its one node.
      services.workbench.front = {
        enable = true;
        adminGroups = [ "workbench-admins" ];
        # A test can't wait five minutes for a session to be checked.
        sessionRefresh = "15s";
        selfService = {
          enable = true;
          groups = [ "workbench" ];
          os = [ "ubuntu" ];
          maxDiskGiB = 32;
          # The host's memory: no VM here boots with it, since one could
          # then take what the other needs to grow.
          maxMemoryGiB = "auto";
          environmentCommand = "${environmentCommand}";
          ownerCommand = "${ownerCommand}";
        };
        oidc = {
          provider = "oidc";
          # No email scope, so the ID token names no email, as Keycloak's
          # doesn't for a directory user without a mail.
          scope = "openid profile groups offline_access";
          issuerUrl = "https://${domain}/dex";
          clientSecretFile = "${pkgs.writeText "client-secret" "test-client-secret"}";
          cookieSecretFile = "${pkgs.writeText "cookie-secret" "0123456789abcdef0123456789abcdef"}";
        };
        virtualHost = {
          forceSSL = true;
          sslCertificate = "${certs}/server.crt";
          sslCertificateKey = "${certs}/server.key";
        };
      };
      services.workbench.node = {
        enable = true;
        name = "host";
        network.externalInterface = "eth1";
      };
      services.workbench.fleet = {
        inherit domain;
        nodes.host = {
          subnet = "10.232.0";
          wireguard.address = "10.231.0.10";
        };
        environment.WORKSPACE_TEST = "shared";
        memory = {
          bootMiB = 2048;
          maxGiB = 4;
        };
        workspaces = lib.mapAttrs (
          name: ws:
          ws
          // {
            node = "host";
            diskGiB = if ws.os or "ubuntu" == "nixos" then 16 else 8;
            cpus = 2;
            inherit apps;
            environment.WORKSPACE_OWNER = ws.owner;
            sshAuthorizedKeys = [ (builtins.readFile "${sshKey}/id.pub") ];
          }
          // lib.optionalAttrs (name == "alice") {
            extraPackages = [ pkgs.hello ];
            allowedEndpoints = [
              {
                address = clientIP;
                port = 8080;
              }
            ];
          }
        ) workspaces;
      };

      systemd.services.alice-secret = {
        requiredBy = [ "workbench@alice.service" ];
        before = [ "workbench@alice.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0700 /var/lib/workbench-secrets/alice
          printf 'WORKSPACE_SECRET=%s\n' "$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')" \
            > /var/lib/workbench-secrets/alice/environment
        '';
      };
    };

    client =
      { nodes, ... }:
      {
        security.pki.certificateFiles = [ "${certs}/ca.crt" ];
        networking.hosts.${nodes.host.networking.primaryIPAddress} = [
          domain
          "alice--nope.${domain}"
          "alice--herdr.${domain}"
        ]
        ++
          lib.concatMap
            (name: [
              "${name}.${domain}"
              "${name}--t3code.${domain}"
            ])
            (
              lib.attrNames workspaces
              ++ [
                "alice-dev"
                "alice-old"
              ]
            );
        networking.interfaces.eth1.ipv4.addresses = lib.mkForce [
          {
            address = clientIP;
            prefixLength = 24;
          }
        ];
        networking.firewall.allowedTCPPorts = [
          8080
          8081
        ];
        environment.systemPackages = [
          pkgs.curl
          pkgs.jq
          pkgs.python3
          pkgs.openssh
          agents.herdr
          agents.herdr-core
        ];
        # alice's and bob's computers, which reach their workspaces through
        # the host.
        users.users = lib.genAttrs [ "alice" "bob" ] (_: {
          isNormalUser = true;
        });
        systemd.services = lib.genAttrs [ "web-8080" "web-8081" ] (name: {
          wantedBy = [ "multi-user.target" ];
          serviceConfig.ExecStart = "${lib.getExe pkgs.python3} -m http.server ${lib.removePrefix "web-" name}";
        });
      };
  };

  testScript = ''
    import json
    import re
    import shlex

    workspaces = ${builtins.toJSON workspaces}
    domain = "${domain}"
    ssh_opts = "-i /root/id -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=30 -o BatchMode=yes"

    start_all()
    host.succeed("test -c /dev/kvm || (echo 'this test needs nested KVM' >&2; false)")
    host.succeed("install -m 0600 ${sshKey}/id /root/id")

    def guest(name, cmd, check=True):
        ws = workspaces[name]
        run = host.succeed if check else host.fail
        return run(f"ssh {ssh_opts} {ws['owner']}@{ws['ip']} {shlex.quote(cmd)}")

    def url(name):
        return f"https://{name}--t3code.{domain}"

    # What the front says of a workspace: its record, and what waits for
    # its next start, which the front tells from the record and the VM.
    def record_of(name):
        return json.loads(host.succeed(f"cat /var/lib/workbench/registry/{name}.json"))

    def front_of(name):
        seen = json.loads(host.succeed(
            "curl -sf --unix-socket /run/workbench-web/ui.sock -H 'X-Workbench-User: carol' -H 'X-Workbench-Groups: workbench-admins' http://ui/api/workspaces"
        ))
        return next(w for w in seen if w["name"] == name)

    def os_of(name):
        return workspaces[name].get("os", "ubuntu")

    for name, ws in workspaces.items():
        with subtest(f"{name} boots {os_of(name)}"):
            host.wait_for_unit(f"workbench@{name}.service")
            host.wait_until_succeeds(f"ssh {ssh_opts} {ws['owner']}@{ws['ip']} true", timeout=900)
            release = guest(name, "cat /etc/os-release")
            assert ("Ubuntu" if os_of(name) == "ubuntu" else "NixOS") in release, release
            if os_of(name) == "ubuntu":
                guest(name, "cloud-init status --wait >/dev/null")
            # A NixOS guest copies the app's closure first.
            units = " ".join(f"workspace-app-{a}.service" for a in ${builtins.toJSON apps})
            guest(name, f"timeout 900 sh -c 'until systemctl is-active {units} workspace-agent.service; do sleep 5; done'")

    with subtest("the front and its node reach each other through sockets, without WireGuard"):
        host.fail("ip link show wg-workbench")
        assert host.succeed("stat -c '%a %U %G' /run/workbench-node/api.sock").strip() == "660 root workbench"
        assert host.succeed("stat -c '%a %U' /run/workbench-front/api.sock").strip() == "600 root"
        for name in workspaces:
            record = json.loads(host.succeed(f"cat /var/lib/workbench/registry/{name}.json"))
            assert record["node"] == "host" and record["declared"], record

    with subtest("the agent runs the app as the owner with its environment"):
        for name, ws in workspaces.items():
            pid = guest(name, "systemctl show -P MainPID workspace-app-t3code.service").strip()
            assert guest(name, f"stat -c %U /proc/{pid}").strip() == ws["owner"]
            env = guest(name, f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n'")
            assert f"WORKSPACE_OWNER={ws['owner']}" in env, env
            assert "WORKSPACE_TEST=shared" in env, env
            assert f"APP_URL={url(name)}" in env and "APP_PORT=3773" in env, env
            guest(name, "sudo -n true")

    with subtest("the guest is shared only its closure"):
        for name in workspaces:
            config = json.loads(host.succeed(f"cat /etc/workbench/{name}.json"))
            closure = sorted(p.removeprefix("/nix/store/") for p in host.succeed(f"cat {config['closure']}").split())
            shared = "/nix/store" if os_of(name) == "ubuntu" else "/run/workspace/host/nix/store"
            seen = sorted(guest(name, f"ls -A {shared}").split())
            assert seen == closure, f"{name}: {set(seen) ^ set(closure)}"
            assert len(host.succeed("ls /nix/store").split()) > len(closure)
            guest(name, f"! sudo touch {shared}/probe")
        guest("alice", "timeout 60 bash -lc hello")
        guest("bob", "! bash -lc 'command -v hello'")

    with subtest("a NixOS workspace is its owner's to rebuild"):
        config = json.loads(host.succeed("cat /etc/workbench/bob.json"))
        packages = guest("bob", "readlink /run/workspace/config/app").strip()
        assert packages == config["packages"], packages
        guest("bob", f"nix-store --verify-path {packages}")
        assert guest("bob", "readlink /nix/var/nix/gcroots/workspace-app").strip() == packages
        agent = guest("bob", "readlink /run/workspace/config/agent").strip()
        assert agent == config["agent"], agent
        assert guest("bob", "readlink /nix/var/nix/gcroots/workspace-agent").strip() == agent
        bundle = config["apps"][0]["bundle"]
        assert guest("bob", "readlink /nix/var/nix/gcroots/workspace-app-t3code").strip() == bundle
        assert guest("bob", "hostname").strip() == "bob"
        flake = guest("bob", "cat /etc/nixos/flake.nix")
        assert 'nixosConfigurations."bob"' in flake and "workbench.nixosModules.guest" in flake, flake
        assert 'users.users."bob"' in guest("bob", "cat /etc/nixos/configuration.nix")
        guest("bob", "id -nG | grep -qw wheel")
        guest("bob", "echo '# kept' | sudo tee -a /etc/nixos/configuration.nix >/dev/null")

    with subtest("secrets reach the guest but not the Nix store"):
        secret = host.succeed("sed -n 's/^WORKSPACE_SECRET=//p' /var/lib/workbench-secrets/alice/environment").strip()
        assert len(secret) == 48
        pid = guest("alice", "systemctl show -P MainPID workspace-app-t3code.service").strip()
        guest("alice", f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n' | grep -qx WORKSPACE_SECRET={secret}")
        guest("bob", "! test -s /run/workspace/secrets/environment")
        config = json.loads(host.succeed("cat /etc/workbench/alice.json"))
        # The secret is made after the system was built, so it could only
        # reach the store at run time, in the store's writable layer.
        host.succeed("test -d /nix/.rw-store")
        host.fail(f"grep -RlsF {secret} /nix/.rw-store /etc/workbench /etc/systemd/system {config['seed']}")

    def status_code(cmd):
        return client.succeed(f"curl -s -o /dev/null -w '%{{http_code}}' {cmd}").strip()

    def as_user(user):
        return f"-b /tmp/sso-{user}.jar"

    # A form posted from the UI, as a browser without JavaScript sends it.
    ui_form = "-H 'Accept: text/html' -H 'Origin: https://${domain}'"

    with subtest("nobody reaches the UI or a workspace without signing in"):
        host.wait_for_unit("workbench-web.service")
        host.wait_until_succeeds("curl -sf https://${domain}/dex/.well-known/openid-configuration")
        host.wait_until_succeeds("curl -s -o /dev/null -w '%{http_code}' https://${domain}/ | grep -qx 302", timeout=120)
        for target in ["https://${domain}/", url("alice") + "/", url("alice") + "/.well-known/t3/environment"]:
            out = client.succeed(f"curl -s -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {target}")
            assert out.startswith("302 https://") and "/oauth2/start?rd=https://" in out, out
        # A workspace's own name leads to its panel, where its apps are.
        out = client.succeed("curl -s -o /dev/null -w '%{http_code} %{redirect_url}' https://alice.${domain}/x")
        assert out == "302 https://${domain}/?w=alice", out

    with subtest("signing in shows each person their own workspaces"):
        for user in ("alice", "bob", "carol"):
            page = client.succeed(f"python3 ${ssoLogin} {user} https://${domain}/")
            assert page.startswith("https://${domain}/"), page[:200]
            mine = [name for name, ws in workspaces.items() if ws["owner"] == user]
            for name in workspaces:
                shown = f'data-ws="{name}"' in page
                assert shown == (user == "carol" or name in mine), (user, name, page[:1500])
            assert ('href="/host"' in page) == (user == "carol"), user
        # The live updates come through nginx unbuffered.
        events = client.succeed(f"curl -sN -m 3 {as_user('alice')} https://${domain}/events || true")
        assert "event: rows" in events, events

    with subtest("a workspace lets in only its owner and admins"):
        for user, name, code in (
            ("alice", "alice", "200"), ("alice", "bob", "403"),
            ("bob", "bob", "200"), ("bob", "alice", "403"),
            ("carol", "alice", "200"), ("carol", "bob", "200"),
        ):
            client.wait_until_succeeds(
                f"curl -s -o /dev/null -w '%{{http_code}}' {as_user(user)} {url(name)}/.well-known/t3/environment | grep -qx {code}",
                timeout=300,
            )
        denied = client.succeed(f"curl -s {as_user('bob')} {url('alice')}/")
        assert "isn't yours" in denied and "bob" in denied, denied
        assert status_code(f"{as_user('alice')} https://alice--nope.${domain}/") == "404"
        assert "has no app" in client.succeed(f"curl -s {as_user('alice')} https://alice--nope.${domain}/")
        # The app gets the browser's cookies without the SSO one.
        sso = client.succeed("awk '$6 == \"_workbench_auth\" { print $7 }' /tmp/sso-alice.jar").strip()
        assert sso
        out = host.succeed(
            "curl -s -D - -o /dev/null --unix-socket /run/workbench-web/authz.sock "
            f"-H 'X-Workbench-Workspace: alice' -H 'X-Workbench-App: t3code' -H 'Cookie: other=1; _workbench_auth={sso}' http://ui/authz"
        )
        assert "x-workbench-cookie: other=1\r\n" in out.lower(), out

    with subtest("the UI sees each VM through its node"):
        seen = json.loads(host.succeed(
            "curl -sf --unix-socket /run/workbench-web/ui.sock -H 'X-Workbench-User: carol' -H 'X-Workbench-Groups: workbench-admins' http://ui/api/workspaces"
        ))
        assert sorted(ws["name"] for ws in seen) == sorted(workspaces), seen
        for ws in seen:
            assert ws["state"] == "running" and ws["apps"][0]["openLink"] and not ws.get("changes"), ws

    with subtest("the serial console is shared by the node, and the VM's sockets are no one else's"):
        # The node holds each VM's serial socket, and hands it to
        # `workbench console` and to the front, which relays it to the UI.
        host.succeed("timeout 60 python3 ${consoleLogin} alice")
        assert host.succeed("stat -c '%a %U' /run/workbench/alice/vm").strip() == "700 wb-alice"
        # Refused for want of permission, not for any other reason.
        host.succeed(
            "runuser -u workbench-web -- python3 -c '"
            "import socket, sys\n"
            "try: socket.socket(socket.AF_UNIX).connect(\"/run/workbench/alice/vm/serial.sock\")\n"
            "except PermissionError: sys.exit(0)\n"
            "sys.exit(1)'"
        )

    with subtest("the UI opens an app in a browser it signs in"):
        open_ = "-d ws=alice -d app=t3code https://${domain}/api/open"
        out = client.succeed(f"curl -s -o /dev/null -D - {as_user('alice')} {ui_form} {open_}")
        assert out.startswith("HTTP/2 303") or out.startswith("HTTP/1.1 303"), out
        assert f"location: {url('alice')}/pair#token=" in out.lower(), out
        assert status_code(f"{as_user('bob')} {ui_form} {open_}") == "403"
        assert status_code(f"{as_user('alice')} -H 'Accept: text/html' -H 'Origin: {url('alice')}' {open_}") == "403"

    cookies = {}
    for name in workspaces:
        with subtest(f"{name} signs a browser in over HTTPS"):
            owner = workspaces[name]["owner"]
            def session_state(cookie=None, name=name, owner=owner):
                header = f"-H 'Cookie: {cookie}'" if cookie else as_user(owner)
                return json.loads(client.succeed(
                    f"curl -sf -H 'Origin: {url(name)}' {header} {url(name)}/api/auth/session"
                ))
            client.wait_until_succeeds(f"curl -sf {as_user(owner)} {url(name)}/.well-known/t3/environment", timeout=300)
            assert not session_state()["authenticated"]
            link = host.succeed(f"workbench open-link {name}").strip()
            prefix = f"{url(name)}/pair#token="
            assert link.startswith(prefix), link
            grant = {"credential": link.removeprefix(prefix)}
            client.succeed(
                f"curl -sf {as_user(owner)} -c /tmp/{name}.jar -H 'Origin: {url(name)}' "
                f"-H 'Content-Type: application/json' "
                f"-d '{json.dumps({'credential': grant['credential']})}' "
                f"{url(name)}/api/auth/browser-session"
            )
            cookie_name = session_state()["auth"]["sessionCookieName"]
            value = client.succeed(f"awk '$6 == \"{cookie_name}\" {{ print $7 }}' /tmp/{name}.jar").strip()
            assert value, f"no {cookie_name} cookie for {name}"
            cookies[name] = (cookie_name, value)
            sso = client.succeed(f"awk '$6 == \"_workbench_auth\" {{ print $7 }}' /tmp/sso-{owner}.jar").strip()
            assert session_state(f"_workbench_auth={sso}; {cookie_name}={value}")["authenticated"]

    with subtest("a session does not open another workspace"):
        # An admin passes SSO everywhere, so only the app's own session
        # stands between a workspace and another's.
        admin = client.succeed("awk '$6 == \"_workbench_auth\" { print $7 }' /tmp/sso-carol.jar").strip()
        for owner, other in (("alice", "bob"), ("bob", "alice")):
            name = json.loads(client.succeed(
                f"curl -sf -H 'Origin: {url(other)}' {as_user('carol')} {url(other)}/api/auth/session"
            ))["auth"]["sessionCookieName"]
            state = json.loads(client.succeed(
                f"curl -sf -H 'Origin: {url(other)}' -H 'Cookie: _workbench_auth={admin}; {name}={cookies[owner][1]}' {url(other)}/api/auth/session"
            ))
            assert not state["authenticated"]

    # A person's computer: a user of the client's, alice or bob, who runs
    # what they would in a terminal, without one.
    def laptop(user, cmd, check=True, err=True):
        run = client.succeed if check else client.fail
        return run(f"su - {user} -c {shlex.quote(cmd)} </dev/null" + (" 2>&1" if err else ""))

    def connect_code(user, name, app):
        page = client.succeed(f"curl -sf {as_user(user)} https://${domain}/w/{name}/connect/{app}")
        found = re.search(r"sh -s -- (wbp-[0-9a-f]{32})", page)
        assert found, page[:3000]
        return found.group(1)

    with subtest("the host's SSH CAs are each readable by its signer only"):
        perms = host.succeed("stat -c '%n %U %a' /var/lib/workbench/ssh/host-ca /var/lib/workbench/ssh/user-ca").split("\n")
        assert perms[:2] == ["/var/lib/workbench/ssh/host-ca root 400", "/var/lib/workbench/ssh/user-ca workbench-web 400"], perms
        for name in workspaces:
            cert = guest(name, "ssh-keygen -L -f /run/workspace/secrets/ssh-host/ssh_host_ed25519_key-cert.pub")
            assert "host certificate" in cert and f"{name}.${domain}" in cert, cert
            guest(name, "! cat /run/workspace/secrets/ssh-host/ssh_host_ed25519_key")

    with subtest("the panel has Connect for an app reached from a terminal"):
        page = client.succeed(f"curl -sf {as_user('alice')} 'https://${domain}/?w=alice'")
        assert 'href="/w/alice/connect/herdr"' in page and 'href="/w/alice/connect/herdrcore"' in page, page[:3000]
        page = client.succeed(f"curl -sf {as_user('alice')} https://${domain}/w/alice/connect/herdr")
        assert "curl -fsSL https://${domain}/connect/install.sh | sh -s -- wbp-" in page and "herdr --remote alice.${domain}" in page, page[:3000]
        assert status_code(f"{as_user('bob')} https://${domain}/w/alice/connect/herdr") == "403"
        # Herdr has no address, so nothing reaches it through nginx.
        assert status_code(f"{as_user('alice')} https://alice--herdr.${domain}/") == "404"

    with subtest("alice sets her computer up with a code from it"):
        code = connect_code("alice", "alice", "herdr")
        out = laptop("alice", f"curl -fsSL https://${domain}/connect/install.sh | sh -s -- {code}")
        assert "ssh <workspace>.${domain}" in out, out
        laptop("alice", "test -x ~/.local/bin/workbench-connect")
        laptop("alice", "head -1 ~/.ssh/config | grep -qx 'Include ~/.config/workbench/ssh_config'")
        assert laptop("alice", "stat -c %a ~/.config/workbench/credential").strip() == "600"
        laptop("alice", "grep -qx '@cert-authority \\*.${domain} ssh-ed25519 .*' ~/.config/workbench/known_hosts")
        out = laptop("alice", f"~/.local/bin/workbench-connect setup --server https://${domain} {code}", check=False)
        assert "doesn't work" in out, "a code works once: " + out

    with subtest("ssh reaches her workspace through the tunnel, by certificates"):
        # Only the host CA is known, and only her certificate lets her in.
        out = laptop("alice", "ssh -o BatchMode=yes alice.${domain} 'id -un; hostname'", err=False)
        assert out.split() == ["alice", "alice"], out
        guest("alice", "sudo journalctl -b | grep -q 'Accepted publickey for alice .* ED25519-CERT'")
        devices = client.succeed(f"curl -sf {as_user('alice')} https://${domain}/devices")
        assert "<b>client</b>" in devices and "never used" not in devices, devices[:3000]

    with subtest("bob's credential reaches his workspace, not alice's"):
        code = connect_code("bob", "bob", "herdr")
        laptop("bob", f"curl -fsSL https://${domain}/connect/install.sh | sh -s -- {code}")
        assert laptop("bob", "ssh -o BatchMode=yes bob.${domain} id -un", err=False).strip() == "bob"
        out = laptop("bob", "ssh -o BatchMode=yes alice.${domain} true", check=False)
        assert "isn't yours" in out, out
        out = laptop("bob", "ssh -o BatchMode=yes -l alice alice.${domain} true", check=False)
        assert "isn't yours" in out, out

    owners = {"alice": "alice", "bob": "bob"}
    for name, user in owners.items():
        with subtest(f"herdr in {name}: its server runs, and herdr reaches it without a terminal"):
            guest(name, "systemctl is-active workspace-app-herdr.service")
            out = laptop(user, f"herdr machine add {name}.${domain} --label {name} --remote-session default")
            assert "Remote server is ready" in out, out
            status = json.loads(laptop(user, f"herdr --machine {name} status server --json", err=False))
            assert status["running"] and status["capabilities"]["detached_server_daemon"], status
            made = json.loads(laptop(user, f"herdr --machine {name} workspace create --cwd /home/{user} --label probe --no-focus", err=False))
            pane = made["result"]["root_pane"]["pane_id"]
            probe = "printf 'PROBE=%s:%s\\n' \"$WORKSPACE_OWNER\" \"$WORKSPACE_TEST\""
            laptop(user, f"herdr --machine {name} pane run {pane} {shlex.quote(probe)}")
            laptop(user, f"herdr --machine {name} pane wait-output {pane} --regex '^PROBE={user}:shared$' --timeout 30000")
            out = laptop(user, f"ssh {name}.${domain} herdr update", check=False)
            assert "comes from the workspace's host" in out, out

        with subtest(f"Herdr app in {name}: the unit started the core, which SSH sees"):
            guest(name, "systemctl is-active workspace-app-herdrcore.service")
            # Asked before any hc command over SSH, which would start one.
            units = laptop(user, f"ssh {name}.${domain} systemctl --user list-units --plain --no-legend 'hc-serve-*'", err=False)
            assert units.startswith("hc-serve-"), units
            pid = laptop(user, f"ssh {name}.${domain} systemctl --user show -P MainPID {units.split()[0]}", err=False).strip()
            status = json.loads(laptop(user, f"ssh {name}.${domain} hc server status --json", err=False))
            assert str(status["pid"]) == pid, (status, pid)
            listed = json.loads(laptop(user, f"ssh {name}.${domain} hc ls", err=False))
            assert "terminals" in listed, listed
            assert laptop(user, f"ssh {name}.${domain} hc run printenv WORKSPACE_OWNER", err=False).strip() == user
            # As herdr-app adds it: a machine of the computer's own core.
            laptop(user, f"hc machine add {name}.${domain}")
            assert laptop(user, f"hc run --on {name} printenv WORKSPACE_TEST", err=False).strip() == "shared"
            out = laptop(user, f"ssh {name}.${domain} hc update", check=False)
            assert "comes from the workspace's host" in out, out

    with subtest("a pane of either runtime has the workspace's secrets"):
        secret = host.succeed("sed -n 's/^WORKSPACE_SECRET=//p' /var/lib/workbench-secrets/alice/environment").strip()
        assert laptop("alice", "hc run --on alice printenv WORKSPACE_SECRET", err=False).strip() == secret
        made = json.loads(laptop("alice", "herdr --machine alice workspace create --cwd /home/alice --label secret --no-focus", err=False))
        pane = made["result"]["root_pane"]["pane_id"]
        probe = "printf 'SECRET=%s\\n' \"$WORKSPACE_SECRET\""
        laptop("alice", f"herdr --machine alice pane run {pane} {shlex.quote(probe)}")
        laptop("alice", f"herdr --machine alice pane wait-output {pane} --regex '^SECRET={secret}$' --timeout 30000")

    with subtest("a revoked credential is refused"):
        devices = client.succeed(f"curl -sf {as_user('bob')} https://${domain}/devices")
        found = re.search(r'data-device="([0-9a-f]{16})"', devices)
        assert found, devices[:3000]
        device = found.group(1)
        assert status_code(f"{as_user('alice')} {ui_form} -d id={device} https://${domain}/api/revoke-device") == "303"
        assert device in client.succeed(f"curl -sf {as_user('bob')} https://${domain}/devices"), "not alice's"
        assert status_code(f"{as_user('bob')} {ui_form} -d id={device} https://${domain}/api/revoke-device") == "303"
        out = laptop("bob", "ssh -o BatchMode=yes bob.${domain} true", check=False)
        assert "revoked" in out, out
        assert laptop("alice", "ssh -o BatchMode=yes alice.${domain} true") == "", "alice's still works"

    with subtest("guests reach neither the host, each other nor private ranges"):
        def blocked(name, ip, port):
            guest(name, f"! timeout 5 bash -c '</dev/tcp/{ip}/{port}'")
        def open_(name, ip, port):
            guest(name, f"timeout 5 bash -c '</dev/tcp/{ip}/{port}'")
        host_lan = host.succeed("ip -4 -o addr show eth1 | awk '{print $4}' | cut -d/ -f1").strip()
        for name, other in (("alice", "bob"), ("bob", "alice")):
            for port in (22, 443, 80):
                blocked(name, "10.232.0.1", port)
                blocked(name, host_lan, port)
            blocked(name, workspaces[other]["ip"], 22)
            blocked(name, workspaces[other]["ip"], 3773)
            blocked(name, "${clientIP}", 8081)
            guest(name, "! timeout 5 ping -c1 -W2 10.232.0.1")
        open_("alice", "${clientIP}", 8080)
        blocked("bob", "${clientIP}", 8080)
        dev = guest("alice", "ip -o -4 addr show | awk '$4 == \"10.232.0.10/32\" {print $2}'").strip()
        guest("alice", f"sudo ip addr add 10.232.0.11/32 dev {dev}")
        guest("alice", "! curl -sf -m 5 --interface 10.232.0.11 http://${clientIP}:8080/")
        guest("alice", "curl -sf -m 5 --interface 10.232.0.10 http://${clientIP}:8080/ >/dev/null")
        guest("alice", f"sudo ip addr del 10.232.0.11/32 dev {dev}")

    with subtest("workspace memory grows under pressure"):
        guest("alice", "sudo systemd-run --unit=hog python3 -c 'import time; x = b\"x\" * (1000 << 20); time.sleep(600)'")
        host.wait_until_succeeds(
            "workbench status --json | jq -e '.[] | select(.name == \"alice\") | .memoryBytes > (2 * 1073741824)'",
            timeout=120,
        )

    with subtest("auto is the host's memory, as a start reads it"):
        kib = int(host.succeed("awk '/^MemTotal:/ { print $2 }' /proc/meminfo"))
        mib = kib // 1024 // 128 * 128
        host.succeed("mkdir -p /tmp/auto && jq '.maxMemMiB = \"auto\"' /etc/workbench/alice.json > /tmp/auto/alice.json")
        host.succeed(
            "workbench --config-dir /tmp/auto status --json"
            f" | jq -e '.[] | select(.name == \"alice\") | .maxMemoryBytes == {mib << 20}'"
        )

    with subtest("status lists both workspaces"):
        out = host.succeed("workbench status")
        print(out)
        for name in workspaces:
            assert name in out
        for ws in json.loads(host.succeed("workbench status --json")):
            assert ws["state"] == "running" and ws["apps"][0]["openLink"] and not ws.get("changes"), ws

    with subtest("a NixOS workspace restarts with its store and configuration"):
        host.succeed("systemctl restart workbench@bob.service")
        host.wait_until_succeeds(f"ssh {ssh_opts} bob@10.232.0.11 systemctl is-active workspace-app-t3code.service", timeout=600)
        guest("bob", "grep -qx '# kept' /etc/nixos/configuration.nix")
        out = guest("bob", "journalctl -b -u workspace-store.service")
        assert "copying" not in out, out

    with subtest("a stop through the UI lasts until a start"):
        # The web UI asks the node, which stops it and leaves its marker.
        assert status_code(f"{as_user('alice')} {ui_form} -d 'ws=bob&action=stop' https://${domain}/api/control") == "403"
        assert status_code(f"{as_user('bob')} {ui_form} -d 'ws=bob&action=stop' https://${domain}/api/control") == "303"
        host.wait_until_succeeds("systemctl show -P ActiveState workbench@bob.service | grep -qx inactive", timeout=180)
        host.succeed("test -e /var/lib/workbench/.stopped/bob")
        # What a reboot does: start everything wanted by multi-user.target.
        host.succeed("systemctl start workbench@bob.service")
        host.succeed("systemctl show -P ActiveState workbench@bob.service | grep -qx inactive")
        assert json.loads(host.succeed("workbench status --json"))[1]["state"] == "stopped"

    with subtest("a stop powers the guest off and destroy deletes the disk"):
        host.succeed("test -f /var/lib/workbench/bob/disk.raw")
        host.fail("echo nope | workbench destroy bob")
        host.succeed("test -f /var/lib/workbench/bob/disk.raw")
        host.succeed("echo bob | workbench destroy bob")
        host.fail("test -e /var/lib/workbench/bob")
        host.succeed("test -f /var/lib/workbench/alice/disk.raw")

    # Workspaces people make themselves, with the groups from the directory.
    def create(user, name, keys=""):
        form = f"-d name={name} -d os=ubuntu -d memory_gib=4 -d disk_gib=16 -d apps%5B0%5D=t3code --data-urlencode ssh_keys={shlex.quote(keys)}"
        return client.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {as_user(user)} {ui_form} {form} https://${domain}/api/create"
        ).strip()

    with subtest("bob, outside the group, can't make a workspace"):
        assert status_code(f"{as_user('bob')} https://${domain}/new") == "403"
        assert create("bob", "bob-dev").startswith("403"), "bob may not"
        page = client.succeed(f"curl -s {as_user('alice')} https://${domain}/")
        assert 'href="/new"' in page, "alice may"
        # The template's auto, as the web UI offers it.
        page = client.succeed(f"curl -s {as_user('alice')} https://${domain}/new")
        assert f"2 to {mib // 1024}, or auto" in page, page

    with subtest("a failing credential command refuses the request"):
        host.succeed("touch /run/no-credential")
        out = create("alice", "alice-dev")
        assert out.startswith("303") and "/new?error=" in out and "credential" in out, out
        host.fail("test -e /var/lib/workbench/configs/alice-dev.json")
        host.fail("test -e /var/lib/workbench/alice-dev")
        host.succeed("rm /run/no-credential")

    with subtest("alice, in the group, makes a workspace and reaches it"):
        key = host.succeed("cat ${sshKey}/id.pub").strip()
        out = create("alice", "alice-dev", key)
        assert out == "303 https://${domain}/?w=alice-dev", out
        config = json.loads(host.succeed("cat /var/lib/workbench/configs/alice-dev.json"))
        assert config["owner"] == "alice" and config["selfService"] and config["ip"] == "10.232.0.128", config
        record = record_of("alice-dev")
        assert (record["node"], record["ip"], record["owner"]) == ("host", "10.232.0.128", "alice"), record
        assert [a["id"] for a in config["apps"]] == ["t3code"], config
        host.succeed("getent passwd wb-alice-dev")
        workspaces["alice-dev"] = {"ip": config["ip"], "owner": "alice"}
        host.wait_until_succeeds(f"ssh {ssh_opts} alice@{config['ip']} true", timeout=900)
        guest("alice-dev", "cloud-init status --wait >/dev/null")
        guest("alice-dev", "timeout 600 sh -c 'until systemctl is-active workspace-app-t3code.service; do sleep 5; done'")
        pid = guest("alice-dev", "systemctl show -P MainPID workspace-app-t3code.service").strip()
        env = guest("alice-dev", f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n'")
        assert "WORKSPACE_TOKEN=token-of-alice" in env, env
        # One virtual host, no reload: the access check names the address.
        for user, code in (("alice", "200"), ("carol", "200"), ("bob", "403")):
            client.wait_until_succeeds(
                f"curl -s -o /dev/null -w '%{{http_code}}' {as_user(user)} {url('alice-dev')}/.well-known/t3/environment | grep -qx {code}",
                timeout=300,
            )

    def status_of(name):
        return next(w for w in json.loads(host.succeed("workbench status --json")) if w["name"] == name)

    def set_apps(form):
        return client.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {as_user('alice')} {ui_form} "
            f"-d ws=alice-dev {form} https://${domain}/api/apps"
        ).strip()

    def app_active(name):
        return guest(name, "systemctl is-active workspace-app-t3code.service || true").strip() == "active"

    with subtest("alice changes her workspace's apps, which wait for a restart"):
        assert set_apps("") == "303 https://${domain}/?w=alice-dev"
        # The record changes; the node's config, at the next start.
        assert record_of("alice-dev")["apps"] == []
        # The front tells what waits for the restart from the record.
        ws = front_of("alice-dev")
        assert ws["changes"] == [{"kind": "appRemoved", "app": "t3code"}], ws
        assert app_active("alice-dev"), "it runs until the restart"
        page = client.succeed(f"curl -s {as_user('alice')} 'https://${domain}/?w=alice-dev'")
        assert "T3 Code goes with the next restart." in page, page[:3000]
        assert set_apps("-d restart=true") == "303 https://${domain}/?w=alice-dev"
        host.wait_until_succeeds("workbench status --json | jq -e '.[] | select(.name == \"alice-dev\") | .state == \"running\" and (.changes | not)'", timeout=600)
        host.wait_until_succeeds(f"ssh {ssh_opts} alice@{config['ip']} true", timeout=600)
        assert not app_active("alice-dev"), "gone with the restart"
        assert status_code(f"{as_user('alice')} {url('alice-dev')}/") == "404"
        assert set_apps("-d apps%5B0%5D=t3code") == "303 https://${domain}/?w=alice-dev"
        assert front_of("alice-dev")["changes"] == [{"kind": "appAdded", "app": "t3code"}]
        out = client.succeed(f"curl -s -o /dev/null -w '%{{redirect_url}}' {as_user('alice')} {ui_form} -d ws=alice-dev -d app=t3code https://${domain}/api/open")
        assert out == "https://${domain}/w/alice-dev/open/t3code", out
        host.succeed("workbench restart alice-dev")
        host.wait_until_succeeds(f"ssh {ssh_opts} alice@{config['ip']} systemctl is-active workspace-app-t3code.service", timeout=600)
        assert not status_of("alice-dev").get("changes")

    with subtest("a config from before apps gets its app back"):
        file = "/var/lib/workbench/configs/alice-dev.json"
        host.succeed(f"jq '.app = .packages | del(.packages, .apps) | .appPort = 3773' {file} > /tmp/old.json && cp /tmp/old.json {file}")
        host.succeed("systemctl restart workbench-node-setup.service")
        now = json.loads(host.succeed(f"cat {file}"))
        assert [a["id"] for a in now["apps"]] == ["t3code"] and "appPort" not in now and "app" not in now, now
        assert not status_of("alice-dev").get("changes"), "the same app, the same packages"

    with subtest("the quota refuses alice's second workspace"):
        out = create("alice", "alice-two")
        assert "quota" in out, out
        host.fail("test -e /var/lib/workbench/configs/alice-two.json")

    with subtest("leaving the group cuts alice off, and the timer stops her VM"):
        host.succeed(
            "printf 'dn: cn=workbench,ou=groups,${baseDN}\\nchangetype: modify\\ndelete: memberUid\\nmemberUid: alice\\n' "
            "| ldapmodify -x -H ldap://127.0.0.1 -D cn=admin,${baseDN} -w admin"
        )
        # At the next session refresh, the groups come without workbench.
        client.wait_until_succeeds(
            f"curl -s -o /dev/null -w '%{{http_code}}' {as_user('alice')} {url('alice-dev')}/ | grep -qx 403",
            timeout=120,
        )
        assert status_code(f"{as_user('alice')} {url('alice')}/.well-known/t3/environment") == "200", "her declared one stays"
        host.succeed("systemctl start workbench-self-service-reconcile.service")
        host.wait_until_succeeds("systemctl show -P ActiveState workbench@alice-dev.service | grep -qx inactive", timeout=180)
        host.succeed("test -e /var/lib/workbench/.left/alice-dev && test -f /var/lib/workbench/alice-dev/disk.raw")
        # Started again, by anyone: the next pass stops it again.
        host.succeed("workbench start alice-dev")
        host.succeed("systemctl start workbench-self-service-reconcile.service")
        host.wait_until_succeeds("systemctl show -P ActiveState workbench@alice-dev.service | grep -qx inactive", timeout=180)
        page = client.succeed(f"curl -s {as_user('carol')} 'https://${domain}/?w=alice-dev'")
        assert "owner left" in page, page[:2000]

    with subtest("back in the group, her workspace starts again"):
        # Its node keeps it stopped, across a reboot too, until she is back.
        host.succeed("test -e /var/lib/workbench/.stopped/alice-dev")
        host.succeed(
            "printf 'dn: cn=workbench,ou=groups,${baseDN}\\nchangetype: modify\\nadd: memberUid\\nmemberUid: alice\\n' "
            "| ldapmodify -x -H ldap://127.0.0.1 -D cn=admin,${baseDN} -w admin"
        )
        host.succeed("systemctl start workbench-self-service-reconcile.service")
        host.wait_until_succeeds("systemctl show -P ActiveState workbench@alice-dev.service | grep -qx active", timeout=180)
        host.fail("test -e /var/lib/workbench/.left/alice-dev")
        host.fail("test -e /var/lib/workbench/.stopped/alice-dev")

    with subtest("an admin destroys it, and its user and tap go"):
        tap = config["tap"]
        out = client.succeed(
            f"curl -s -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {as_user('carol')} {ui_form} "
            "-d ws=alice-dev -d confirm=alice-dev https://${domain}/api/destroy"
        ).strip()
        assert out == "303 https://${domain}/", out
        host.fail("test -e /var/lib/workbench/configs/alice-dev.json")
        host.fail("test -e /var/lib/workbench/registry/alice-dev.json")
        host.fail("test -e /var/lib/workbench/alice-dev")
        host.fail("getent passwd wb-alice-dev")
        host.fail(f"ip link show {tap}")

    with subtest("a workspace made at run time before the fleet comes through the upgrade"):
        # As a host on its own wrote it, with its secrets, seed and disk,
        # and no record: what a deploy of this release finds there.
        name, ip = "alice-old", "10.232.0.200"
        old = dict(config, name=name, user=f"wb-{name}", url=f"https://{name}.${domain}", ip=ip, uid=60650,
                   stateDir=f"/var/lib/workbench/{name}", runtimeDir=f"/run/workbench/{name}",
                   seed=f"/var/lib/workbench/.seeds/{name}.img", tap="wb0ae800c8", mac="02:00:0a:e8:00:c8",
                   secretsDir=f"/var/lib/workbench-secrets/{name}", socketGroup="workbench",
                   ssh={"keygen": "${pkgs.openssh}/bin/ssh-keygen", "hostCa": "/var/lib/workbench/ssh/host-ca",
                        "userCa": "/var/lib/workbench/ssh/user-ca.pub", "hostKeys": "/var/lib/workbench/ssh/hosts"})
        host.succeed(f"echo {shlex.quote(json.dumps(old))} > /var/lib/workbench/configs/{name}.json")
        host.succeed(
            f"install -d -m 0700 /var/lib/workbench-secrets/{name}"
            f" && echo WORKSPACE_TOKEN=token-of-alice > /var/lib/workbench-secrets/{name}/environment"
            f" && echo seed > {old['seed']} && echo from-main > /var/lib/workbench/.seeds/{name}.instance"
            f" && install -d -m 0711 {old['stateDir']}"
            f" && {old['qemuImg']} convert -f {old['imageFormat']} -O raw {old['image']} {old['stateDir']}/disk.raw"
        )
        host.fail(f"test -e /var/lib/workbench/registry/{name}.json")
        # The deploy: the node's setup puts its user, tap and seed back and
        # starts it, which the front refuses until its setup has its record.
        host.succeed("systemctl restart workbench-node-setup.service workbench-front-setup.service")
        record = record_of(name)
        assert (record["node"], record["ip"], record["owner"]) == ("host", ip, "alice") and not record.get("declared"), record
        workspaces[name] = {"ip": ip, "owner": "alice"}
        host.wait_until_succeeds(f"ssh {ssh_opts} alice@{ip} true", timeout=900)
        guest(name, "cloud-init status --wait >/dev/null")
        guest(name, "timeout 600 sh -c 'until systemctl is-active workspace-app-t3code.service; do sleep 5; done'")
        pid = guest(name, "systemctl show -P MainPID workspace-app-t3code.service").strip()
        assert "WORKSPACE_TOKEN=token-of-alice" in guest(name, f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n'")
        mine = json.loads(client.succeed(f"curl -sf {as_user('alice')} https://${domain}/api/workspaces"))
        assert any(w["name"] == name for w in mine), "listed for its owner"
        client.wait_until_succeeds(
            f"curl -s -o /dev/null -w '%{{http_code}}' {as_user('alice')} {url(name)}/.well-known/t3/environment | grep -qx 200",
            timeout=300,
        )
        # The front takes it away, as the UI's Destroy does.
        host.succeed(f"echo {name} | workbench destroy {name}")
        host.fail(f"test -e /var/lib/workbench/registry/{name}.json")
        host.fail(f"test -e /var/lib/workbench/configs/{name}.json")
        host.fail(f"getent passwd wb-{name}")
  '';
}
