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
      -subj "/CN=workspaces test CA" -keyout ca.key -out $out/ca.crt
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj "/CN=*.${domain}" -keyout $out/server.key -out server.csr
    printf 'subjectAltName=DNS:*.${domain}\n' > ext
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
    };
  };
  clientIP = "192.168.1.3";
in
pkgs.testers.runNixOSTest {
  name = "workspaces";

  nodes = {
    host = {
      imports = [ flake.nixosModules.default ];

      virtualisation = {
        memorySize = 8192;
        cores = 4;
        diskSize = 20480;
      };
      networking.firewall.allowedTCPPorts = [ 443 ];
      environment.systemPackages = [
        pkgs.openssh
        pkgs.jq
      ];

      services.workbench = {
        enable = true;
        inherit domain;
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
            diskGiB = 8;
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
        networking.hosts.${nodes.host.networking.primaryIPAddress} = map (name: "${name}.${domain}") (
          lib.attrNames workspaces
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

    for name, ws in workspaces.items():
        with subtest(f"{name} boots Ubuntu"):
            host.wait_for_unit(f"workbench@{name}.service")
            host.wait_until_succeeds(f"ssh {ssh_opts} {ws['owner']}@{ws['ip']} true", timeout=900)
            assert "Ubuntu" in guest(name, "cat /etc/os-release")
            guest(name, "cloud-init status --wait >/dev/null; systemctl is-active t3code.service workspace-agent.service")

    with subtest("the T3 Code server runs as the owner with its environment"):
        for name, ws in workspaces.items():
            pid = guest(name, "systemctl show -P MainPID t3code.service").strip()
            assert guest(name, f"stat -c %U /proc/{pid}").strip() == ws["owner"]
            env = guest(name, f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n'")
            assert f"WORKSPACE_OWNER={ws['owner']}" in env, env
            assert "WORKSPACE_TEST=shared" in env, env
            guest(name, "sudo -n true")

    with subtest("the guest sees only its closure in /nix/store"):
        for name in workspaces:
            config = json.loads(host.succeed(f"cat /etc/workbench/{name}.json"))
            closure = sorted(p.removeprefix("/nix/store/") for p in host.succeed(f"cat {config['closure']}").split())
            seen = sorted(guest(name, "ls -A /nix/store").split())
            assert seen == closure, f"{name}: {set(seen) ^ set(closure)}"
            assert len(host.succeed("ls /nix/store").split()) > len(closure)
            guest(name, "! touch /nix/store/probe")
        guest("alice", "timeout 60 bash -lc hello")
        guest("bob", "! bash -lc 'command -v hello'")

    with subtest("secrets reach the guest but not the Nix store"):
        secret = host.succeed("sed -n 's/^WORKSPACE_SECRET=//p' /var/lib/workbench-secrets/alice/environment").strip()
        assert len(secret) == 48
        pid = guest("alice", "systemctl show -P MainPID t3code.service").strip()
        guest("alice", f"sudo cat /proc/{pid}/environ | tr '\\0' '\\n' | grep -qx WORKSPACE_SECRET={secret}")
        guest("bob", "! test -s /run/workspace/secrets/environment")
        config = json.loads(host.succeed("cat /etc/workbench/alice.json"))
        # The secret is made after the system was built, so it could only
        # reach the store at run time, in the store's writable layer.
        host.succeed("test -d /nix/.rw-store")
        host.fail(f"grep -RlsF {secret} /nix/.rw-store /etc/workbench /etc/systemd/system {config['seed']}")

    cookies = {}
    for name in workspaces:
        with subtest(f"{name} pairs over HTTPS"):
            def session_state(cookie=None, name=name):
                header = f"-H 'Cookie: {cookie}'" if cookie else ""
                return json.loads(client.succeed(
                    f"curl -sf -H 'Origin: {url(name)}' {header} {url(name)}/api/auth/session"
                ))
            client.wait_until_succeeds(f"curl -sf {url(name)}/.well-known/t3/environment", timeout=300)
            assert not session_state()["authenticated"]
            grant = json.loads(host.succeed(f"workbench pair {name} --json"))
            assert grant["pairUrl"] == f"{url(name)}/pair#token={grant['credential']}", grant
            client.succeed(
                f"curl -sf -c /tmp/{name}.jar -H 'Origin: {url(name)}' "
                f"-H 'Content-Type: application/json' "
                f"-d '{json.dumps({'credential': grant['credential']})}' "
                f"{url(name)}/api/auth/browser-session"
            )
            cookie_name = session_state()["auth"]["sessionCookieName"]
            value = client.succeed(f"awk '$6 == \"{cookie_name}\" {{ print $7 }}' /tmp/{name}.jar").strip()
            assert value, f"no {cookie_name} cookie for {name}"
            cookies[name] = (cookie_name, value)
            assert session_state(f"{cookie_name}={value}")["authenticated"]

    with subtest("a session does not open another workspace"):
        for owner, other in (("alice", "bob"), ("bob", "alice")):
            name = json.loads(client.succeed(
                f"curl -sf -H 'Origin: {url(other)}' {url(other)}/api/auth/session"
            ))["auth"]["sessionCookieName"]
            state = json.loads(client.succeed(
                f"curl -sf -H 'Origin: {url(other)}' -H 'Cookie: {name}={cookies[owner][1]}' {url(other)}/api/auth/session"
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

    with subtest("a stop powers the guest off and destroy deletes the disk"):
        host.succeed("systemctl stop workbench@bob.service")
        host.succeed("test -f /var/lib/workbench/bob/disk.raw")
        host.fail("echo nope | workbench destroy bob")
        host.succeed("test -f /var/lib/workbench/bob/disk.raw")
        host.succeed("echo bob | workbench destroy bob")
        host.fail("test -e /var/lib/workbench/bob")
        host.succeed("test -f /var/lib/workbench/alice/disk.raw")
  '';
}
