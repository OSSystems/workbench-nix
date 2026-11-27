# A front and two nodes, `a` and `b`, over WireGuard, the guests nested as
# checks.workspaces boots them: a workspace made at run time lands on the
# node with the most free memory, a declared one boots on `b`, both ask
# the front, and a move from `b` to `a` keeps the disk. A node gets only
# its own workspaces' starts, nodes never reach each other, even one that
# routes the other's addresses into its tunnel, no guest reaches the
# tunnel or any API, and a node of another build takes no new workspace.
#
# The front signs people in as checks.workspaces does, with OpenLDAP and
# Dex, and a client is their computer: they reach an app on `b` and its
# SSH server through the front, an admin picks, closes and moves nodes in
# the UI, and a node that doesn't answer leaves its apps answering.
#
# A browser on the client takes the UI's screens along the way, into
# `$out/shots`: the pictures of docs/fleet.md, which `nix run
# .#screenshots` copies there.
{
  pkgs,
  flake,
  system,
  ...
}:
let
  inherit (pkgs) lib;
  domain = "ws.test";
  # Every machine's: the front and the nodes are one build.
  inherit (flake.packages.${system}.default) build;

  certs = pkgs.runCommand "multi-node-test-certs" { nativeBuildInputs = [ pkgs.openssl ]; } ''
    mkdir $out
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 3650 \
      -subj "/CN=multi-node test CA" -keyout ca.key -out $out/ca.crt \
      -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj "/CN=${domain}" -keyout $out/server.key -out server.csr
    printf 'subjectAltName=DNS:${domain},DNS:*.${domain}\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' > ext
    openssl x509 -req -in server.csr -CA $out/ca.crt -CAkey ca.key -CAcreateserial \
      -days 3650 -extfile ext -out $out/server.crt
  '';
  sshKey = pkgs.runCommand "multi-node-test-ssh-key" { nativeBuildInputs = [ pkgs.openssh ]; } ''
    mkdir $out
    ssh-keygen -q -t ed25519 -N "" -C test -f $out/id
  '';
  # WireGuard's keys; a store path is no place for them but in a test.
  keys =
    pkgs.runCommand "multi-node-test-wireguard-keys" { nativeBuildInputs = [ pkgs.wireguard-tools ]; }
      ''
        mkdir $out
        for m in front a b; do
          wg genkey > $out/$m.key
          wg pubkey < $out/$m.key > $out/$m.pub
        done
      '';
  publicKey = m: lib.removeSuffix "\n" (builtins.readFile "${keys}/${m}.pub");
  key = lib.removeSuffix "\n" (builtins.readFile "${sshKey}/id.pub");

  environmentCommand = pkgs.writeShellScript "workspace-env" ''
    echo "WORKSPACE_TOKEN=token-of-$1"
  '';

  # The directory: alice makes workspaces of her own, carol is an admin,
  # bob is in neither group.
  baseDN = "dc=ws,dc=test";
  users = [
    "alice"
    "bob"
    "carol"
  ];
  directory = ''
    dn: ${baseDN}
    objectClass: dcObject
    objectClass: organization
    dc: ws
    o: multi-node test

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

  # A browser's console: a WebSocket to the UI's console of a workspace,
  # with the single sign-on cookie, pressing Enter until the guest's login
  # prompt shows.
  consoleOverUi = pkgs.writeText "console-over-ui.py" ''
    import base64, json, os, socket, ssl, struct, sys, time
    host, ws, cookie = sys.argv[1], sys.argv[2], sys.argv[3]
    s = ssl.create_default_context().wrap_socket(socket.create_connection((host, 443)), server_hostname=host)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((
        f"GET /w/{ws}/console/socket HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nOrigin: https://{host}\r\n"
        f"Cookie: _workbench_auth={cookie}\r\n\r\n"
    ).encode())
    head = b""
    while b"\r\n\r\n" not in head:
        head += s.recv(1)
    assert head.startswith(b"HTTP/1.1 101"), head
    def send(data):
        mask = os.urandom(4)
        s.sendall(bytes([0x82, 0x80 | len(data)]) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))
    buf, out, viewers = b"", b"", []
    s.settimeout(1)
    deadline = time.time() + 120
    while b"login:" not in out and time.time() < deadline:
        send(b"\r")
        try:
            buf += s.recv(65536)
        except TimeoutError:
            pass
        while len(buf) >= 2:
            n, at = buf[1] & 0x7f, 2
            if n == 126:
                n, at = struct.unpack(">H", buf[2:4])[0], 4
            elif n == 127:
                n, at = struct.unpack(">Q", buf[2:10])[0], 10
            if len(buf) < at + n:
                break
            op, data, buf = buf[0] & 0x0f, buf[at:at + n], buf[at + n:]
            if op == 2:
                out += data
            elif op == 1:
                viewers = json.loads(data)["viewers"]
    assert b"login:" in out, out[-500:]
    print(json.dumps(viewers))
  '';

  # The client's browser: one headless Firefox, which signs in as each
  # person once, through Dex's form, and keeps their cookies, so a screen
  # is taken in a second or two, while a move is under way too. It takes
  # requests on a socket, a JSON line each: who, the path, the picture's
  # name and, maybe, a text to wait for.
  browserPython = pkgs.python3.withPackages (p: [ p.selenium ]);
  shotd = pkgs.writeText "shotd.py" ''
    import json, os, socket, sys, time
    from selenium import webdriver
    from selenium.webdriver.common.by import By
    from selenium.webdriver.firefox.options import Options
    from selenium.webdriver.firefox.service import Service
    from selenium.webdriver.support.ui import WebDriverWait

    domain, out = sys.argv[1], sys.argv[2]
    os.makedirs(out, exist_ok=True)
    options = Options()
    options.add_argument("-headless")
    options.binary_location = "${pkgs.firefox}/bin/firefox"
    # The test's own CA, which Firefox's store doesn't have.
    options.accept_insecure_certs = True
    options.set_preference("layout.css.prefers-color-scheme.content-override", 1)
    options.set_preference("layout.css.devPixelsPerPx", "2")
    browser = webdriver.Firefox(options=options, service=Service("${pkgs.geckodriver}/bin/geckodriver"))
    browser.set_window_size(1280, 800)
    browser.set_page_load_timeout(60)
    cookies, who = {}, None

    def sign_in(user):
        global who
        if who == user:
            return
        # A page of the domain that needs no one signed in, to set cookies
        # on: Dex's, in plain text, which Firefox shows rather than saves.
        browser.get(f"https://{domain}/dex/healthz")
        browser.delete_all_cookies()
        if user in cookies:
            for c in cookies[user]:
                browser.add_cookie(c)
        else:
            browser.get(f"https://{domain}/")
            WebDriverWait(browser, 60).until(lambda b: b.find_elements(By.NAME, "login"))
            browser.find_element(By.NAME, "login").send_keys(user)
            password = browser.find_element(By.NAME, "password")
            password.send_keys("password")
            password.submit()
            # Dex may still ask to grant the client access, as sso-login.py
            # finds.
            approve = "//input[@name='approval'][@value='approve']/ancestor::form"
            done = lambda b: "/dex/" not in b.current_url
            WebDriverWait(browser, 60).until(lambda b: done(b) or b.find_elements(By.XPATH, approve))
            if not done(browser):
                browser.find_element(By.XPATH, approve).submit()
                WebDriverWait(browser, 60).until(done)
            cookies[user] = browser.get_cookies()
        who = user

    server = socket.socket(socket.AF_UNIX)
    server.bind("/run/shotd.sock")
    server.listen()
    while True:
        conn, _ = server.accept()
        with conn:
            req = json.loads(conn.makefile().readline())
            print(req, flush=True)
            try:
                browser.set_window_size(1280, req["height"])
                sign_in(req["user"])
                browser.get(f"https://{domain}{req['path']}")
                if req["wait"]:
                    WebDriverWait(browser, 60).until(lambda b: req["wait"] in b.page_source)
                # The page hydrates; its fonts settle.
                time.sleep(1.5)
                # A page that scrolls inside, such as the host page's
                # nodes, grows the window until all of it shows.
                for _ in range(4):
                    over = browser.execute_script(
                        "return Math.max(0, ...Array.from(document.querySelectorAll('*'), e => e.scrollHeight - e.clientHeight))"
                    )
                    if over <= 1:
                        break
                    browser.set_window_size(1280, browser.get_window_size()["height"] + over)
                    time.sleep(0.5)
                browser.get_full_page_screenshot_as_file(f"{out}/{req['name']}.png")
                conn.sendall(b"ok\n")
            except Exception as e:
                conn.sendall(f"{req['name']}: {e!r} at {browser.current_url}: {browser.title!r}\n".encode())
  '';
  shot = pkgs.writeText "shot.py" ''
    import json, socket, sys
    user, path, name, wait, height = sys.argv[1:6]
    s = socket.socket(socket.AF_UNIX)
    s.connect("/run/shotd.sock")
    req = {"user": user, "path": path, "name": name, "wait": wait, "height": int(height)}
    s.sendall((json.dumps(req) + "\n").encode())
    answer = s.makefile().readline().strip()
    print(answer)
    sys.exit(answer != "ok")
  '';

  # The host names the client reaches through the front.
  names = [
    domain
    "bob.${domain}"
    "bob--hello.${domain}"
    "alice-dev--hello.${domain}"
    "carol-b--hello.${domain}"
  ];

  fleet =
    { nodes, ... }:
    {
      services.workbench.fleet = {
        inherit domain;
        # One small app, so the guests boot quickly.
        package = null;
        herdr.package = null;
        herdrCore.package = null;
        apps.hello = {
          name = "Hello";
          port = 8080;
          serve = ''
            mkdir -p www
            echo "hello from $(${pkgs.coreutils}/bin/cat /etc/hostname)" > www/index.html
            exec ${pkgs.busybox}/bin/busybox httpd -f -p "$APP_PORT" -h www
          '';
        };
        # Reached from a terminal: its Connect page pairs a computer.
        apps.shell = {
          name = "Shell";
          port = null;
          default = false;
          oneshot = true;
          serve = "true";
        };
        memory = {
          bootMiB = 1024;
          maxGiB = 2;
        };
        # No workspace here is NixOS: its image is never made.
        nixos.image = pkgs.emptyFile;
        front.wireguard = {
          publicKey = publicKey "front";
          endpoint = "${nodes.front.networking.primaryIPAddress}:51820";
        };
        nodes = {
          a = {
            subnet = "10.232.0";
            wireguard = {
              address = "10.231.0.10";
              publicKey = publicKey "a";
            };
          };
          b = {
            subnet = "10.232.1";
            wireguard = {
              address = "10.231.0.11";
              publicKey = publicKey "b";
            };
          };
        };
        workspaces.bob = {
          node = "b";
          owner = "bob";
          ip = "10.232.1.10";
          diskGiB = 8;
          cpus = 2;
          apps = [
            "hello"
            "shell"
          ];
          sshAuthorizedKeys = [ key ];
        };
      };
      environment.systemPackages = [
        pkgs.curl
        pkgs.jq
        pkgs.openssh
        pkgs.wireguard-tools
      ];
    };

  # A node of another build, as `b` pretends to be at the end: its own API
  # behind a proxy that answers with another `Workbench-Version`.
  otherBuild = pkgs.writeText "other-build-nginx.conf" ''
    daemon off;
    user root;
    pid /run/other-build/nginx.pid;
    error_log stderr;
    events { }
    http {
      access_log off;
      client_body_temp_path /run/other-build/body;
      proxy_temp_path /run/other-build/proxy;
      fastcgi_temp_path /run/other-build/fastcgi;
      uwsgi_temp_path /run/other-build/uwsgi;
      scgi_temp_path /run/other-build/scgi;
      server {
        listen 10.231.0.11:7910;
        location / {
          proxy_pass http://unix:/run/other-build/api.sock;
          proxy_hide_header Workbench-Version;
          add_header Workbench-Version another-build always;
        }
      }
    }
  '';

  # More memory on `b`, which sorts last: a new workspace goes there only
  # because it has the most free memory, though it runs `bob`.
  nodeOf = name: memorySize: {
    imports = [
      flake.nixosModules.default
      fleet
    ];
    virtualisation = {
      inherit memorySize;
      cores = 4;
      diskSize = 30720;
    };
    services.workbench.node = {
      enable = true;
      inherit name;
      network.externalInterface = "eth1";
      wireguard.privateKeyFile = "${keys}/${name}.key";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "multi-node";

  nodes = {
    front = {
      imports = [
        flake.nixosModules.default
        fleet
      ];
      virtualisation = {
        memorySize = 2048;
        cores = 2;
      };
      networking.firewall.allowedTCPPorts = [ 443 ];
      security.pki.certificateFiles = [ "${certs}/ca.crt" ];
      # oauth2-proxy reaches Dex through nginx, as a browser does.
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

      # Dex stands in for Keycloak, as in checks.workspaces.
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

      # As a front with both roles does, for its guests' NAT: the tunnel's
      # own rules, not the kernel's default, keep it from routing between
      # its nodes.
      boot.kernel.sysctl."net.ipv4.ip_forward" = 1;
      services.workbench.front = {
        enable = true;
        virtualHost = {
          forceSSL = true;
          sslCertificate = "${certs}/server.crt";
          sslCertificateKey = "${certs}/server.key";
        };
        wireguard.privateKeyFile = "${keys}/front.key";
        adminGroups = [ "workbench-admins" ];
        oidc = {
          provider = "oidc";
          scope = "openid profile groups offline_access";
          issuerUrl = "https://${domain}/dex";
          clientSecretFile = "${pkgs.writeText "client-secret" "test-client-secret"}";
          cookieSecretFile = "${pkgs.writeText "cookie-secret" "0123456789abcdef0123456789abcdef"}";
        };
        selfService = {
          enable = true;
          groups = [ "workbench" ];
          os = [ "ubuntu" ];
          maxDiskGiB = 16;
          environmentCommand = "${environmentCommand}";
        };
      };
      # bob's secret, on the front only.
      systemd.services.bob-secret = {
        wantedBy = [ "multi-user.target" ];
        before = [ "workbench-front.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          install -d -m 0700 /var/lib/workbench-secrets/bob
          printf 'WORKSPACE_SECRET=%s\n' "$(head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n')" \
            > /var/lib/workbench-secrets/bob/environment
        '';
      };
    };
    a = nodeOf "a" 6144;
    b = nodeOf "b" 8192;

    # Their computer, which reaches everything through the front.
    client =
      { nodes, ... }:
      {
        security.pki.certificateFiles = [ "${certs}/ca.crt" ];
        networking.hosts.${nodes.front.networking.primaryIPAddress} = names;
        # The browser's.
        virtualisation.memorySize = 3072;
        fonts.packages = [
          pkgs.hanken-grotesk
          pkgs.jetbrains-mono
        ];
        environment.systemPackages = [
          pkgs.curl
          pkgs.jq
          browserPython
          pkgs.openssh
        ];
        users.users.bob.isNormalUser = true;
      };
  };

  testScript = ''
    import json
    import re
    import shlex

    build = "${build}"
    front_key = "${publicKey "front"}"
    ssh_opts = "-i /root/id -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=30 -o BatchMode=yes"

    start_all()
    for m in (a, b):
        m.succeed("test -c /dev/kvm || (echo 'this test needs nested KVM' >&2; false)")
    for m in (front, a, b):
        m.succeed("install -m 0600 ${sshKey}/id /root/id")

    def guest(machine, login, cmd, check=True):
        run = machine.succeed if check else machine.fail
        return run(f"ssh {ssh_opts} {login} {shlex.quote(cmd)}")

    def blocked(machine, login, ip, port):
        guest(machine, login, f"! timeout 5 bash -c '</dev/tcp/{ip}/{port}'")

    def answer(machine, method, url, version=build):
        out = machine.succeed(
            f"curl -s -m 10 -X {method} -D /tmp/headers -o /tmp/body -w '%{{http_code}}' "
            f"-H 'Workbench-Version: {version}' {url}"
        )
        return out.strip(), machine.succeed("cat /tmp/headers").lower(), machine.succeed("cat /tmp/body")

    def record_of(name):
        return json.loads(front.succeed(f"cat /var/lib/workbench/registry/{name}.json"))

    def lan(machine):
        return machine.succeed("ip -4 -o addr show eth1 | awk '{print $4}' | cut -d/ -f1").strip()

    def create(request):
        name = request["name"]
        front.succeed(
            f"echo {shlex.quote(json.dumps(request))} > /var/lib/workbench/self-service/requests/{name}.json"
        )
        # A refused request fails its unit; its outcome says why.
        front.execute(f"systemctl start workbench-create@{name}.service")
        return json.loads(front.succeed(f"cat /var/lib/workbench/self-service/results/{name}.json"))

    # What the front dropped of what came in through the tunnel to go on.
    def dropped(machine):
        for line in machine.succeed("iptables -w -nvx -L FORWARD").splitlines():
            f = line.split()
            if len(f) > 6 and f[2] == "DROP" and f[5] == "wg-workbench":
                return int(f[0])
        return 0

    # What a node's tunnel got from the front that the front didn't send:
    # WireGuard counts as a frame error a packet whose source isn't its
    # peer's.
    def not_the_fronts(machine):
        return int(machine.succeed("cat /sys/class/net/wg-workbench/statistics/rx_frame_errors"))

    front.wait_for_unit("workbench-front.service")
    front.wait_for_unit("workbench-front-setup.service")
    for m in (a, b):
        m.wait_for_unit("workbench-node.service")

    with subtest("WireGuard is up, hub and spoke"):
        for node, address in (("a", "10.231.0.10"), ("b", "10.231.0.11")):
            front.wait_until_succeeds(
                f"curl -sf -m 5 -H 'Workbench-Version: {build}' http://{address}:7910/host >/dev/null", timeout=180
            )
        handshakes = front.succeed("wg show wg-workbench latest-handshakes").split()
        assert len(handshakes) == 4 and all(int(t) > 0 for t in handshakes[1::2]), handshakes
        # A node's API is on WireGuard only, and the front's too.
        front.fail(f"curl -s -m 5 http://{lan(a)}:7910/host")
        b.fail(f"curl -s -m 5 -X POST http://{lan(front)}:7911/start/bob")

    with subtest("a request of another build is refused, both ways"):
        code, headers, body = answer(b, "POST", "http://10.231.0.1:7911/start/bob", version="another-build")
        assert code == "409" and f"workbench-version: {build}" in headers, (code, headers, body)
        code, headers, body = answer(front, "GET", "http://10.231.0.11:7910/host", version="another-build")
        assert code == "409" and f"workbench-version: {build}" in headers, (code, headers, body)
        code, headers, body = answer(front, "GET", "http://10.231.0.11:7910/host")
        assert code == "200" and json.loads(body)["version"] == build, (code, body)

    with subtest("only root, and on the front the web UI, reach the other side's API"):
        # Every local user has the machine's WireGuard address: the API
        # tells no one apart by it, the tunnel's owner rules do.
        start = f"curl -s -m 5 -o /dev/null -w '%{{http_code}}' -X POST -H 'Workbench-Version: {build}' http://10.231.0.1:7911/start/bob"
        assert b.succeed(start).strip() == "200"
        b.fail(f"runuser -u nobody -- {start}")
        host = f"curl -sf -m 5 -o /dev/null -H 'Workbench-Version: {build}' http://10.231.0.11:7910/host"
        front.succeed(host)
        front.succeed(f"runuser -u workbench-web -- {host}")
        front.fail(f"runuser -u nginx -- {host}")

    with subtest("a declared workspace boots on its node, with what the front gave it"):
        b.wait_for_unit("workbench@bob.service")
        b.wait_until_succeeds(f"ssh {ssh_opts} bob@10.232.1.10 true", timeout=900)
        guest(b, "bob@10.232.1.10", "cloud-init status --wait >/dev/null")
        guest(b, "bob@10.232.1.10", "timeout 600 sh -c 'until systemctl is-active workspace-agent.service workspace-app-hello.service; do sleep 5; done'")
        secret = front.succeed("sed -n 's/^WORKSPACE_SECRET=//p' /var/lib/workbench-secrets/bob/environment").strip()
        assert len(secret) == 48
        guest(b, "bob@10.232.1.10", f"sudo grep -qx WORKSPACE_SECRET={secret} /run/workspace/secrets/environment")
        # The front's host CA signed its host key; the node has neither CA.
        cert = guest(b, "bob@10.232.1.10", "ssh-keygen -L -f /run/workspace/secrets/ssh-host/ssh_host_ed25519_key-cert.pub")
        ca = front.succeed("ssh-keygen -lf /var/lib/workbench/ssh/host-ca.pub").split()[1]
        assert "host certificate" in cert and "bob.${domain}" in cert and ca in cert, (cert, ca)
        b.fail("test -e /var/lib/workbench/ssh")
        # Nothing of its secrets reaches the node's disk.
        b.fail(f"grep -rlsF {secret} /var/lib/workbench /etc")
        assert record_of("bob")["node"] == "b"

    with subtest("the front reaches a node's guests through the tunnel; the other node doesn't"):
        front.wait_until_succeeds("curl -sf -m 5 http://10.232.1.10:8080/ | grep -q 'hello from bob'", timeout=300)
        front.succeed(f"ssh {ssh_opts} bob@10.232.1.10 true")
        a.fail("curl -s -m 5 http://10.232.1.10:8080/")
        a.fail("curl -s -m 5 http://10.231.0.11:7910/host")

    with subtest("a workspace made at run time lands on the node with the most free memory"):
        outcome = create({
            "name": "alice-dev", "owner": "alice", "os": "ubuntu", "sshAuthorizedKeys": ["${key}"],
            "maxMemoryGiB": 2, "diskGiB": 16, "apps": ["hello"],
        })
        assert outcome["ok"], outcome
        record = record_of("alice-dev")
        assert (record["node"], record["ip"], record.get("pending")) == ("b", "10.232.1.128", None), record
        b.succeed("test -f /var/lib/workbench/configs/alice-dev.json")
        a.fail("test -e /var/lib/workbench/configs/alice-dev.json")
        b.wait_until_succeeds(f"ssh {ssh_opts} alice@10.232.1.128 true", timeout=900)
        guest(b, "alice@10.232.1.128", "cloud-init status --wait >/dev/null")
        guest(b, "alice@10.232.1.128", "sudo grep -qx WORKSPACE_TOKEN=token-of-alice /run/workspace/secrets/environment")
        front.wait_until_succeeds("curl -sf -m 5 http://10.232.1.128:8080/ | grep -q 'hello from alice-dev'", timeout=300)

    with subtest("a node gets only its own workspaces' starts"):
        for name in ("alice-dev", "bob"):
            code, _, body = answer(a, "POST", f"http://10.231.0.1:7911/start/{name}")
            assert code == "403" and "is on node b, not a" in body, (code, body)

    with subtest("guests reach neither the tunnel, nor another node, nor any API"):
        for login in ("alice@10.232.1.128", "bob@10.232.1.10"):
            for ip, port in (
                ("10.231.0.1", 7911), ("10.231.0.10", 7910), ("10.231.0.11", 7910),
                ("10.231.0.1", 22), (lan(front), 443), (lan(a), 22), (lan(b), 22),
            ):
                blocked(b, login, ip, port)
            guest(b, login, "! timeout 5 ping -c1 -W2 10.231.0.1")
        blocked(b, "bob@10.232.1.10", "10.232.1.128", 8080)

    with subtest("a move keeps the disk's data, changes the address and deletes the source's disk"):
        guest(b, "alice@10.232.1.128", "echo kept > ~/before-move && sync")
        out = front.succeed("workbench move alice-dev a 2>&1", timeout=1800)
        assert "alice-dev is on a." in out, out
        record = record_of("alice-dev")
        assert (record["node"], record["ip"], record.get("move")) == ("a", "10.232.0.128", None), record
        b.fail("test -e /var/lib/workbench/alice-dev")
        b.fail("test -e /var/lib/workbench/configs/alice-dev.json")
        b.fail("getent passwd wb-alice-dev")
        a.succeed("test -f /var/lib/workbench/alice-dev/disk.raw")
        a.wait_until_succeeds(f"ssh {ssh_opts} alice@10.232.0.128 true", timeout=900)
        assert guest(a, "alice@10.232.0.128", "cat ~/before-move").strip() == "kept"
        front.wait_until_succeeds("curl -sf -m 5 http://10.232.0.128:8080/ | grep -q 'hello from alice-dev'", timeout=300)
        # Now a's to start: b asking for a's workspace is refused.
        code, _, body = answer(b, "POST", "http://10.231.0.1:7911/start/alice-dev")
        assert code == "403" and "is on node a, not b" in body, (code, body)

    with subtest("a guest on the other node reaches neither the tunnel nor any API"):
        for ip, port in (("10.231.0.1", 7911), ("10.231.0.10", 7910), ("10.231.0.11", 7910), ("10.232.1.10", 8080)):
            blocked(a, "alice@10.232.0.128", ip, port)

    # The UI as people use it, from their computer, signed in.
    def as_user(user):
        return f"-b /tmp/sso-{user}.jar"

    # A form posted from the UI, as a browser without JavaScript sends it.
    ui_form = "-H 'Accept: text/html' -H 'Origin: https://${domain}'"

    def post(user, path, form, timeout=60):
        out = client.succeed(
            f"curl -s -m {timeout} -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {as_user(user)} {ui_form} {form} https://${domain}{path}",
            timeout=timeout + 30,
        )
        return out.strip()

    def listed(user):
        return {w["name"]: w for w in json.loads(client.succeed(f"curl -sf {as_user(user)} https://${domain}/api/workspaces"))}

    def app_of(user, name):
        return client.succeed(f"curl -s -m 10 {as_user(user)} https://{name}--hello.${domain}/")

    # A picture of a screen for docs/fleet.md, as `user` sees it, in a
    # window 1280 wide and at least `height` high: less for a page whose
    # few lines it centres.
    def screen(user, path, name, wait="", height=800):
        client.succeed(f"python3 ${shot} {user} {shlex.quote(path)} {name} {shlex.quote(wait)} {height}", timeout=180)

    def make(user, name, node=""):
        form = (
            f"-d name={name} -d os=ubuntu -d memory_gib=2 -d disk_gib=16 -d apps%5B0%5D=hello "
            f"-d node={node} --data-urlencode ssh_keys={shlex.quote('${key}')}"
        )
        return post(user, "/api/create", form, timeout=900)

    with subtest("signed in, people reach an app of a workspace on b through the front"):
        front.wait_for_unit("workbench-web.service")
        front.wait_until_succeeds("curl -sf https://${domain}/dex/.well-known/openid-configuration", timeout=120)
        front.wait_until_succeeds("curl -s -o /dev/null -w '%{http_code}' https://${domain}/ | grep -qx 302", timeout=120)
        out = client.succeed("curl -s -o /dev/null -w '%{http_code} %{redirect_url}' https://bob--hello.${domain}/")
        assert out.startswith("302 https://") and "/oauth2/start?rd=" in out, out
        for user in ("alice", "bob", "carol"):
            page = client.succeed(f"python3 ${ssoLogin} {user} https://${domain}/")
            assert page.startswith("https://${domain}/") and "/dex/" not in page.split()[0], page[:300]
            client.succeed(f"awk '$6 == \"_workbench_auth\"' /tmp/sso-{user}.jar | grep -q .")
        client.wait_until_succeeds(f"curl -sf {as_user('bob')} https://bob--hello.${domain}/ | grep -q 'hello from bob'", timeout=120)
        assert "hello from bob" in app_of("carol", "bob"), "an admin reaches it too"
        assert "isn't yours" in app_of("alice", "bob")
        client.succeed("systemd-run --unit=shotd --setenv=HOME=/root python3 ${shotd} ${domain} /tmp/shots")
        client.wait_for_file("/run/shotd.sock")
        screen("alice", "/", "list", wait="alice-dev")
        screen("carol", "/?w=bob", "panel-admin", wait="bob")
        screen("carol", "/host", "host", wait="Close b")

    with subtest("ssh reaches b's guest through workbench-connect's tunnel"):
        screen("bob", "/w/bob/connect/shell", "connect", wait="sh -s --", height=720)
        page = client.succeed(f"curl -sf {as_user('bob')} https://${domain}/w/bob/connect/shell")
        found = re.search(r"sh -s -- (wbp-[0-9a-f]{32})", page)
        assert found, page[:3000]
        setup = f"curl -fsSL https://${domain}/connect/install.sh | sh -s -- {found.group(1)}"
        out = client.succeed(f"su - bob -c {shlex.quote(setup)} </dev/null 2>&1")
        assert "ssh <workspace>.${domain}" in out, out
        out = client.succeed("su - bob -c 'ssh -o BatchMode=yes bob.${domain} \"id -un; hostname\"' </dev/null")
        assert out.split() == ["bob", "bob"], out
        guest(b, "bob@10.232.1.10", "sudo journalctl -b | grep -q 'Accepted publickey for bob .* ED25519-CERT'")
        screen("bob", "/devices", "devices", wait="Your devices", height=520)

    with subtest("the console of b's guest comes through the front"):
        sso = client.succeed("awk '$6 == \"_workbench_auth\" { print $7 }' /tmp/sso-bob.jar").strip()
        viewers = json.loads(client.succeed(f"timeout 150 python3 ${consoleOverUi} ${domain} bob {shlex.quote(sso)}"))
        assert viewers == ["bob"], viewers

    with subtest("an admin's node pick in New workspace lands on b"):
        page = client.succeed(f"curl -sf {as_user('carol')} https://${domain}/new")
        assert '<option value="a"' in page and '<option value="b"' in page, page[:3000]
        assert 'name="node"' not in client.succeed(f"curl -sf {as_user('alice')} https://${domain}/new"), "owners pick no node"
        # b has more memory free than a: hold some of it, so placement
        # would take a, and only the pick puts the workspace on b.
        def free(address):
            h = json.loads(answer(front, "GET", f"http://{address}:7910/host")[2])
            return h["memAvailable"] - h["reserve"]
        gib = 1 << 30
        a_free, b_free = free("10.231.0.10"), free("10.231.0.11")
        assert b_free > a_free > 1.3 * gib, (a_free, b_free)
        target = (a_free + gib) // 2
        b.succeed(
            f"systemd-run --unit=hold-memory ${pkgs.python3}/bin/python3 -c "
            f"'import time; x = bytearray({b_free - target}); [x.__setitem__(i, 1) for i in range(0, len(x), 4096)]; time.sleep(3600)'"
        )
        front.wait_until_succeeds(f"curl -sf -m 5 -H 'Workbench-Version: {build}' http://10.231.0.11:7910/host | jq -e '.memAvailable - .reserve < {target + gib // 4}'", timeout=60)
        client.wait_until_succeeds(f"curl -sf {as_user('carol')} https://${domain}/new | grep -q '<option value=\"a\" selected'", timeout=60)
        screen("carol", "/new", "new-admin", wait='<option value="b"')
        out = make("carol", "carol-b", node="b")
        b.succeed("systemctl stop hold-memory")
        assert out == "303 https://${domain}/?w=carol-b", out
        record = record_of("carol-b")
        assert record["node"] == "b" and record["ip"].startswith("10.232.1."), record
        b.succeed("test -f /var/lib/workbench/configs/carol-b.json")
        # Its record is the front's: its node destroys it only through it.
        out = b.fail("echo carol-b | workbench destroy carol-b 2>&1")
        assert "destroy it on the front" in out, out
        carol_b = record["ip"]
        b.wait_until_succeeds(f"ssh {ssh_opts} carol@{carol_b} true", timeout=900)
        client.wait_until_succeeds(f"curl -sf {as_user('carol')} https://carol-b--hello.${domain}/ | grep -q 'hello from carol-b'", timeout=600)
        assert listed("carol")["carol-b"]["node"] == "b"

    with subtest("with b's workbench-node stopped, its workspaces are Unreachable, and their apps still answer"):
        b.succeed("systemctl stop workbench-node.service")
        client.wait_until_succeeds(
            f"curl -sf {as_user('bob')} https://${domain}/api/workspaces | jq -e '.[] | select(.name == \"bob\") | .state == \"unreachable\"'",
            timeout=60,
        )
        seen = listed("carol")
        assert seen["bob"]["state"] == seen["carol-b"]["state"] == "unreachable", seen
        assert seen["alice-dev"]["state"] != "unreachable", seen["alice-dev"]
        assert "node" not in listed("bob")["bob"], "owners see nothing of nodes"
        assert "hello from bob" in app_of("bob", "bob")
        assert "hello from carol-b" in app_of("carol", "carol-b")
        screen("bob", "/", "unreachable", wait="nreachable")
        screen("carol", "/host", "host-unreachable", wait="nreachable")
        b.succeed("systemctl start workbench-node.service")
        client.wait_until_succeeds(
            f"curl -sf {as_user('bob')} https://${domain}/api/workspaces | jq -e '.[] | select(.name == \"bob\") | .state == \"running\"'",
            timeout=120,
        )

    with subtest("with b closed on the host page, new workspaces go to a"):
        assert post("carol", "/api/node", "-d node=b -d closed=true") == "303 https://${domain}/host"
        host = client.succeed(f"curl -sf {as_user('carol')} https://${domain}/host")
        assert "Reopen b" in host and "Close a" in host, host[:3000]
        screen("carol", "/host", "host-closed", wait="Reopen b")
        assert post("alice", "/api/node", "-d node=a -d closed=true").startswith("403"), "for admins"
        assert post("carol", "/api/quota", "-d user=carol -d quota=3") == "303 https://${domain}/host"
        out = make("carol", "carol-a")
        assert out == "303 https://${domain}/?w=carol-a", out
        assert record_of("carol-a")["node"] == "a", "b has more free memory, but is closed"
        # On the front, workbench destroy takes it as the UI's Destroy does.
        front.fail("echo carol-x | workbench destroy carol-a")
        front.succeed("test -e /var/lib/workbench/registry/carol-a.json")
        out = front.succeed("echo carol-a | workbench destroy carol-a 2>&1", timeout=600)
        assert "Deleted carol-a" in out, out
        front.fail("test -e /var/lib/workbench/registry/carol-a.json")
        front.fail("test -e /var/lib/workbench-secrets/carol-a")
        a.fail("test -e /var/lib/workbench/carol-a")
        a.fail("getent passwd wb-carol-a")
        assert post("carol", "/api/node", "-d node=b -d closed=false") == "303 https://${domain}/host"
        assert "Close b" in client.succeed(f"curl -sf {as_user('carol')} https://${domain}/host")

    with subtest("Move from the UI takes a workspace to the other node"):
        assert post("alice", "/api/move", "-d ws=carol-b -d node=a").startswith("403"), "for admins"
        guest(b, f"carol@{carol_b}", "echo kept > ~/before-move && sync")
        screen("carol", "/w/carol-b/move", "move", wait="carol-b", height=520)
        # The form's post, in the background, to see the workspace moving.
        request = (
            f"${pkgs.curl}/bin/curl -s -m 1500 -o /dev/null -w '%{{http_code}} %{{redirect_url}}' {as_user('carol')} {ui_form} "
            f"-d ws=carol-b -d node=a https://${domain}/api/move > /tmp/move.out"
        )
        client.succeed(f"systemd-run --unit=ui-move --remain-after-exit sh -c {shlex.quote(request)}")
        client.wait_until_succeeds(
            f"curl -sf {as_user('carol')} https://${domain}/api/workspaces | jq -e '.[] | select(.name == \"carol-b\") | .state == \"moving\"'",
            timeout=300,
        )
        screen("carol", "/?w=carol-b", "moving", wait="Moving")
        client.wait_until_succeeds("test -s /tmp/move.out", timeout=1500)
        out = client.succeed("cat /tmp/move.out").strip()
        assert out == "303 https://${domain}/host", out
        record = record_of("carol-b")
        assert record["node"] == "a" and record["ip"].startswith("10.232.0.") and not record.get("move"), record
        b.fail("test -e /var/lib/workbench/carol-b")
        a.wait_until_succeeds(f"ssh {ssh_opts} carol@{record['ip']} true", timeout=900)
        assert guest(a, f"carol@{record['ip']}", "cat ~/before-move").strip() == "kept"
        client.wait_until_succeeds(f"curl -sf {as_user('carol')} https://carol-b--hello.${domain}/ | grep -q 'hello from carol-b'", timeout=600)
        assert listed("carol")["carol-b"]["node"] == "a"
        # Gone again, so a has the room it had for what follows.
        assert post("carol", "/api/destroy", "-d ws=carol-b -d confirm=carol-b", timeout=600) == "303 https://${domain}/"
        front.fail("test -e /var/lib/workbench/registry/carol-b.json")

    with subtest("a node that routes the other's addresses into the tunnel gets nothing through the front"):
        # As a node broken into could: the front's address no longer the
        # only one its tunnel takes.
        others = ["10.231.0.10/32", "10.232.0.0/24", f"{lan(a)}/32"]
        b.succeed(f"wg set wg-workbench peer {front_key} allowed-ips " + ",".join(["10.231.0.1/32"] + others))
        for net in others:
            b.succeed(f"ip route replace {net} dev wg-workbench")
        before = (dropped(front), not_the_fronts(a))
        b.fail("curl -s -m 5 http://10.231.0.10:7910/host")
        b.fail("curl -s -m 5 http://10.232.0.128:8080/")
        b.fail(f"ping -c 2 -W 2 {lan(a)}")
        after = (dropped(front), not_the_fronts(a))
        assert after[0] > before[0] and after[1] == before[1], (before, after)
        for net in others:
            b.succeed(f"ip route del {net} dev wg-workbench")
        b.succeed(f"wg set wg-workbench peer {front_key} allowed-ips 10.231.0.1/32")

    with subtest("a node of another build takes no new workspace, though it has the most free memory"):
        b.succeed("systemctl stop workbench-node.service")
        b.succeed(
            "mkdir -p /run/other-build && jq 'del(.listen) | .socket = \"/run/other-build/api.sock\" | .socketGroup = \"\"' "
            "/etc/workbench-node.json > /run/other-build/settings.json"
        )
        b.succeed("systemd-run --unit=other-build-node $(command -v workbench) node --settings /run/other-build/settings.json serve")
        b.wait_until_succeeds("test -S /run/other-build/api.sock")
        b.succeed("systemd-run --unit=other-build-proxy ${pkgs.nginx}/bin/nginx -c ${otherBuild}")
        b.wait_for_open_port(7910, "10.231.0.11")
        code, headers, _ = answer(front, "GET", "http://10.231.0.11:7910/host")
        assert code == "200" and "workbench-version: another-build" in headers, (code, headers)
        request = {
            "name": "carol-dev", "owner": "carol", "os": "ubuntu", "sshAuthorizedKeys": ["${key}"],
            "maxMemoryGiB": 2, "diskGiB": 16, "apps": [],
        }
        outcome = create(dict(request, node="b"))
        assert not outcome["ok"] and "Node b takes no new workspaces" in outcome["error"], outcome
        front.fail("test -e /var/lib/workbench/registry/carol-dev.json")
        outcome = create(request)
        assert outcome["ok"], outcome
        assert record_of("carol-dev")["node"] == "a"

    # Into $out/shots.
    client.copy_from_vm("/tmp/shots")
  '';
}
