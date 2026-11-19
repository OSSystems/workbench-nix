{
  pkgs,
  flake,
  ...
}:
let
  inherit (pkgs) lib;
  domain = "ws.test";

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
  clientIP = "192.168.1.3";
  users = [
    "alice"
    "bob"
    "carol"
  ];

  # Attaches to a workspace's shared console the way `workbench console`
  # does, and presses Enter until the guest's login prompt shows.
  consoleLogin = pkgs.writeText "console-login.py" ''
    import socket, sys, time
    s = socket.socket(socket.AF_UNIX)
    s.connect("/run/workbench-web/console.sock")
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
    done = web.open(action, urllib.parse.urlencode({"login": f"{user}@${domain}", "password": "password"}).encode())
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
        memorySize = 8192;
        cores = 4;
        diskSize = 30720;
      };
      networking.firewall.allowedTCPPorts = [ 443 ];
      environment.systemPackages = [
        pkgs.openssh
        pkgs.jq
        pkgs.curl
        pkgs.python3
      ];
      security.pki.certificateFiles = [ "${certs}/ca.crt" ];
      networking.hosts."127.0.0.1" = [ domain ];

      # Dex stands in for Keycloak, with the same claim for the user.
      services.dex = {
        enable = true;
        settings = {
          issuer = "https://${domain}/dex";
          storage.type = "memory";
          web.http = "127.0.0.1:5556";
          enablePasswordDB = true;
          oauth2.skipApprovalScreen = true;
          staticClients = [
            {
              id = "workbench";
              name = "workbench";
              redirectURIs = [ "https://${domain}/oauth2/callback" ];
              secret = "test-client-secret";
            }
          ];
          # The password of each is "password".
          staticPasswords = map (user: {
            email = "${user}@${domain}";
            hash = "$2a$10$2b2cU8CPhOTaGrs1HRQuAueS7JTT5ZHsHSzYiFPm1leZck7Mc8T4W";
            username = user;
            preferredUsername = user;
            userID = user;
          }) users;
        };
      };
      services.nginx.virtualHosts.${domain}.locations."/dex/".proxyPass = "http://127.0.0.1:5556";

      services.workbench = {
        enable = true;
        inherit domain;
        web = {
          enable = true;
          admins = [ "carol" ];
          oidc = {
            provider = "oidc";
            issuerUrl = "https://${domain}/dex";
            clientSecretFile = "${pkgs.writeText "client-secret" "test-client-secret"}";
            cookieSecretFile = "${pkgs.writeText "cookie-secret" "0123456789abcdef0123456789abcdef"}";
          };
        };
        virtualHost = {
          forceSSL = true;
          sslCertificate = "${certs}/server.crt";
          sslCertificateKey = "${certs}/server.key";
        };
        network.externalInterface = "eth1";
        environment.WORKSPACE_TEST = "shared";
        memory = {
          bootMiB = 2048;
          maxGiB = 4;
        };
        workspaces = lib.mapAttrs (
          name: ws:
          ws
          // {
            diskGiB = if ws.os or "ubuntu" == "nixos" then 16 else 8;
            cpus = 2;
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
        ]
        ++ map (name: "${name}.${domain}") (lib.attrNames workspaces);
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
        ];
        systemd.services = lib.genAttrs [ "web-8080" "web-8081" ] (name: {
          wantedBy = [ "multi-user.target" ];
          serviceConfig.ExecStart = "${lib.getExe pkgs.python3} -m http.server ${lib.removePrefix "web-" name}";
        });
      };
  };

  testScript = ''
    import json
    import shlex

    workspaces = ${builtins.toJSON workspaces}
    domain = "${domain}"
    ssh_opts = "-i /root/id -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o BatchMode=yes"

    start_all()
    host.succeed("test -c /dev/kvm || (echo 'this test needs nested KVM' >&2; false)")
    host.succeed("install -m 0600 ${sshKey}/id /root/id")

    def guest(name, cmd, check=True):
        ws = workspaces[name]
        run = host.succeed if check else host.fail
        return run(f"ssh {ssh_opts} {ws['owner']}@{ws['ip']} {shlex.quote(cmd)}")

    def url(name):
        return f"https://{name}.{domain}"

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
            guest(name, "timeout 900 sh -c 'until systemctl is-active workspace-app.service workspace-agent.service; do sleep 5; done'")

    with subtest("the app runs as the owner with its environment"):
        for name, ws in workspaces.items():
            pid = guest(name, "systemctl show -P MainPID workspace-app.service").strip()
            assert guest(name, f"stat -c %U /proc/{pid}").strip() == ws["owner"]
            env = guest(name, f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n'")
            assert f"WORKSPACE_OWNER={ws['owner']}" in env, env
            assert "WORKSPACE_TEST=shared" in env, env
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
        app = guest("bob", "readlink /run/workspace/config/app").strip()
        assert app == config["app"], app
        guest("bob", f"nix-store --verify-path {app}")
        assert guest("bob", "readlink /nix/var/nix/gcroots/workspace-app").strip() == app
        assert guest("bob", "hostname").strip() == "bob"
        flake = guest("bob", "cat /etc/nixos/flake.nix")
        assert 'nixosConfigurations."bob"' in flake and "workbench.nixosModules.guest" in flake, flake
        assert 'users.users."bob"' in guest("bob", "cat /etc/nixos/configuration.nix")
        guest("bob", "id -nG | grep -qw wheel")
        guest("bob", "echo '# kept' | sudo tee -a /etc/nixos/configuration.nix >/dev/null")

    with subtest("secrets reach the guest but not the Nix store"):
        secret = host.succeed("sed -n 's/^WORKSPACE_SECRET=//p' /var/lib/workbench-secrets/alice/environment").strip()
        assert len(secret) == 48
        pid = guest("alice", "systemctl show -P MainPID workspace-app.service").strip()
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
        # The app gets the browser's cookies without the SSO one.
        sso = client.succeed("awk '$6 == \"_workbench_auth\" { print $7 }' /tmp/sso-alice.jar").strip()
        assert sso
        out = host.succeed(
            "curl -s -D - -o /dev/null --unix-socket /run/workbench-web/authz.sock "
            f"-H 'X-Workbench-Workspace: alice' -H 'Cookie: other=1; _workbench_auth={sso}' http://ui/authz"
        )
        assert "x-workbench-cookie: other=1\r\n" in out.lower(), out

    with subtest("the UI sees each VM through its sockets"):
        seen = json.loads(host.succeed(
            "curl -sf --unix-socket /run/workbench-web/ui.sock -H 'X-Workbench-User: carol' http://ui/api/workspaces"
        ))
        for ws in seen:
            assert ws["state"] == "running" and ws["signIn"], ws

    with subtest("the serial console is shared through the UI"):
        # The UI holds each VM's serial socket, which share-sockets opened
        # to its group, and hands it to `workbench console` on the host.
        host.succeed("timeout 60 python3 ${consoleLogin} alice")
        perms = host.succeed("stat -c '%a %G' /run/workbench/alice/vm/serial.sock /run/workbench/alice/vm/api.sock").split("\n")
        assert all(p == "660 workbench" for p in perms if p), perms

    with subtest("the UI signs a browser in to the app"):
        out = client.succeed(
            f"curl -s -o /dev/null -D - {as_user('alice')} {ui_form} -d ws=alice https://${domain}/api/sign-in"
        )
        assert out.startswith("HTTP/2 303") or out.startswith("HTTP/1.1 303"), out
        assert f"location: {url('alice')}/pair#token=" in out.lower(), out
        assert status_code(f"{as_user('bob')} {ui_form} -d ws=alice https://${domain}/api/sign-in") == "403"
        assert status_code(
            f"{as_user('alice')} -H 'Accept: text/html' -H 'Origin: {url('alice')}' -d ws=alice https://${domain}/api/sign-in"
        ) == "403"

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
            link = host.succeed(f"workbench sign-in {name}").strip()
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

    with subtest("status lists both workspaces"):
        out = host.succeed("workbench status")
        print(out)
        for name in workspaces:
            assert name in out
        for ws in json.loads(host.succeed("workbench status --json")):
            assert ws["state"] == "running" and ws["signIn"] and not ws.get("appUpdate"), ws

    with subtest("a NixOS workspace restarts with its store and configuration"):
        host.succeed("systemctl restart workbench@bob.service")
        host.wait_until_succeeds(f"ssh {ssh_opts} bob@10.232.0.11 systemctl is-active workspace-app.service", timeout=600)
        guest("bob", "grep -qx '# kept' /etc/nixos/configuration.nix")
        out = guest("bob", "journalctl -b -u workspace-store.service")
        assert "copying" not in out, out

    with subtest("a stop through the UI lasts until a start"):
        # The web UI controls units through polkit, as its own user.
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
  '';
}
