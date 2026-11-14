{
  pkgs,
  self,
  serverPackage,
}:
let
  alternateBrowser = pkgs.writeShellScript "stravia-alternate-browser" ''
    exec ${pkgs.lib.getExe pkgs.chromium} "$@"
  '';
  consumer = pkgs.writeText "stravia-service-consumer.py" ''
    import http.cookiejar
    import json
    import re
    import ssl
    import subprocess
    import sys
    import urllib.error
    import urllib.request

    mode, port = sys.argv[1], sys.argv[2]
    host = sys.argv[3] if len(sys.argv) > 3 else "127.0.0.1"
    tls = len(sys.argv) > 4
    origin = ("https://" if tls else "http://") + host + ":" + port
    cookies = http.cookiejar.CookieJar()
    handlers = [urllib.request.HTTPCookieProcessor(cookies)]
    if tls:
        handlers.append(urllib.request.HTTPSHandler(
            context=ssl.create_default_context(cafile=sys.argv[4]),
        ))
    client = urllib.request.build_opener(*handlers)

    def request(method, path, payload=None, headers=None):
        supplied = {"Origin": origin, "x-stravia-csrf": "1"}
        supplied.update(headers or {})
        if payload is not None:
            supplied["Content-Type"] = "application/json"
        req = urllib.request.Request(
            origin + path,
            data=None if payload is None else json.dumps(payload).encode(),
            headers=supplied, method=method,
        )
        try:
            response = client.open(req, timeout=180)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            raw = response.read()
            return response.status, json.loads(raw) if raw else None

    if mode == "initialize":
        status, state = request("GET", "/api/v1/auth/state")
        assert status == 200 and state["mode"] == "setup", (status, state)
        journal = subprocess.check_output(
            ["journalctl", "-u", "stravia.service", "--no-pager", "-o", "cat"], text=True,
        )
        token = re.findall(r"Stravia setup token:\s*(\S+)", journal)[-1]
        # 伪造转发头不能让环境文件中的代理信任生效；失败响应不得泄露 token。
        status, _ = request("POST", "/api/v1/setup/claim", {"token": token}, {
            "Origin": "https://injected.invalid",
            "X-Forwarded-Host": "injected.invalid",
            "X-Forwarded-Proto": "https",
        })
        assert status == 403, status
        status, _ = request("POST", "/api/v1/setup/claim", {"token": token})
        assert status == 204, status
        status, body = request("POST", "/api/v1/setup/complete", {
            "client_base_url": origin,
            "database": {"backend": "sqlite"},
            "username": "vm-owner", "password": "isolated VM test password",
        })
        assert status == 200, body
    # 设置完成不会自动登录；首次启动与重启均通过真实管理员认证。
    status, body = request("POST", "/api/v1/auth/login", {
        "username": "vm-owner", "password": "isolated VM test password",
    })
    assert status == 200, body
    assert request("GET", "/readyz")[0] == 200
    assert request("GET", "/api/v1/status")[0] == 200
    status, vendors = request("GET", "/api/v1/vendor-plugins")
    assert status == 200, vendors
    expected_vendors = {
        "base", "openai-codex", "xai-grok", "command-code", "devin",
        "opencode-free", "claude-code", "antigravity",
    }
    bundled = [v for v in vendors["data"] if v["vendor_id"] in expected_vendors]
    assert len(bundled) == len(expected_vendors), vendors
    assert {v["vendor_id"] for v in bundled} == expected_vendors, vendors
    assert all(v["source"] == "builtin" and v["status"] == "ready" for v in bundled), vendors
  '';
  common = { ... }: {
    imports = [ (import ../nixos-module.nix { inherit self; }) ];
    services.stravia = {
      enable = true;
      package = serverPackage;
    };
    environment.systemPackages = [
      pkgs.python3
      pkgs.curl
    ];
    # 测试 VLAN 已有静态地址；不让额外 DHCP/RA 在 Chromium 导航中改变网络。
    networking.useDHCP = false;
    virtualisation.memorySize = 3072;
    virtualisation.cores = 2;
  };
in
pkgs.testers.runNixOSTest {
  name = "stravia-service";
  nodes = {
    default = { ... }: {
      imports = [ common ];
      services.stravia.environmentFile = "/run/stravia env %$.env";
      # 运行时生成的是隔离测试值；没有真实凭据或从 store 复制生产配置。
      systemd.services.stravia-test-environment = {
        before = [ "stravia.service" ];
        requiredBy = [ "stravia.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          umask 077
          printf '%s\n' \
            'STRAVIA_TRUSTED_PROXIES=127.0.0.1/32' \
            'STRAVIA_ADMIN_ORIGINS=https://injected.invalid' \
            'STRAVIA_PORT=0' \
            > '/run/stravia env %$.env'
        '';
      };
    };
    custom = { ... }: {
      imports = [ common ];
      users.groups.operator = { };
      users.users.operator = {
        isSystemUser = true;
        group = "operator";
      };
      services.stravia = {
        user = "operator";
        group = "operator";
        dataDir = "/srv/stravia %$ state";
        configFile = "/srv/stravia %$ state/custom.toml";
        host = "::";
        port = 24567;
        openFirewall = true;
        adminOrigins = [
          "http://127.0.0.1:24567"
          "http://[::1]:24567"
          "https://localhost:8443"
        ];
        trustedProxies = [ "127.0.0.1/32" ];
        configPollInterval = 0;
        browserPackage = pkgs.chromium;
        environmentFile = "/run/stravia-browser.env";
      };
      systemd.services.stravia-browser-environment = {
        before = [ "stravia.service" ];
        requiredBy = [ "stravia.service" ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = "umask 077; touch /run/stravia-browser.env";
      };
      # 测试私钥只在隔离 VM 的 /run 生成，不进入 Nix store；浏览器正常验证证书。
      systemd.services.stravia-test-pki = {
        before = [
          "nginx.service"
          "stravia.service"
        ];
        requiredBy = [
          "nginx.service"
          "stravia.service"
        ];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          RuntimeDirectory = "stravia-test-pki";
          RuntimeDirectoryMode = "0755";
        };
        path = [ pkgs.openssl ];
        script = ''
          umask 077
          cd /run/stravia-test-pki
          openssl req -x509 -newkey rsa:2048 -noenc -sha256 -days 1 \
            -keyout ca.key -out ca.pem -subj '/CN=Stravia VM root' \
            -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign'
          openssl req -newkey rsa:2048 -noenc -sha256 \
            -keyout server.key -out server.csr -subj '/CN=localhost'
          openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
            -out server.pem -days 1 -sha256 \
            -extfile <(printf '%s\n' 'subjectAltName=DNS:localhost' 'extendedKeyUsage=serverAuth')
          chmod 0644 ca.pem server.pem
          chgrp nginx server.key
          chmod 0640 server.key
        '';
      };
      services.nginx = {
        enable = true;
        virtualHosts.localhost = {
          onlySSL = true;
          listen = [
            {
              addr = "127.0.0.1";
              port = 8443;
              ssl = true;
            }
          ];
          sslCertificate = "/run/stravia-test-pki/server.pem";
          sslCertificateKey = "/run/stravia-test-pki/server.key";
          locations."/" = {
            proxyPass = "http://127.0.0.1:24567";
            extraConfig = ''
              proxy_set_header Host localhost:8443;
              proxy_set_header X-Forwarded-Host localhost:8443;
              proxy_set_header X-Forwarded-Proto https;
            '';
          };
        };
      };
      systemd.services.stravia.after = [ "nginx.service" ];
      systemd.services.stravia.requires = [ "nginx.service" ];
      systemd.services.stravia.serviceConfig.ExecStartPost =
        pkgs.writeShellScript "stravia-browser-smoke" ''
          set -euo pipefail
          if [ ! -f "$HOME/.pki/nssdb/cert9.db" ]; then
            mkdir -p "$HOME/.pki/nssdb"
            ${pkgs.nssTools}/bin/certutil -N -d "sql:$HOME/.pki/nssdb" --empty-password
            ${pkgs.nssTools}/bin/certutil -A -d "sql:$HOME/.pki/nssdb" \
              -n stravia-vm-root -t 'C,,' -i /run/stravia-test-pki/ca.pem
          fi
          "$STRAVIA_CHROME_PATH" --headless --disable-gpu --dump-dom \
            "data:text/html,<title>stravia-nixos-browser</title><script>document.title='stravia-javascript-executed'</script>" \
            | ${pkgs.python3}/bin/python3 -c 'import sys; assert "<title>stravia-javascript-executed</title>" in sys.stdin.read()'
          "$STRAVIA_CHROME_PATH" --headless --disable-gpu --dump-dom https://localhost:8443/ \
            | ${pkgs.python3}/bin/python3 -c 'import sys; assert "/_app/immutable/" in sys.stdin.read()'
        '';
    };
    missing = { ... }: {
      imports = [ common ];
      services.stravia.environmentFile = "/run/does-not-exist.env";
    };
  };
  testScript = ''
    start_all()
    for machine, port, root, config_name, owner in [
        (default, 23471, "/var/lib/stravia", "server.toml", "stravia"),
        (custom, 24567, "/srv/stravia %$ state", "custom.toml", "operator"),
    ]:
        import shlex
        machine.wait_for_unit("stravia.service")
        machine.succeed("test $(systemctl show stravia.service -p NRestarts --value) = 0")
        machine.wait_until_succeeds(f"curl -fsS http://127.0.0.1:{port}/healthz")
        machine.succeed(f"python3 ${consumer} initialize {port}")
        machine.succeed(f"test $(stat -c %a {shlex.quote(root)}) = 700")
        machine.succeed(f"test $(stat -c %U:%G {shlex.quote(root)}) = {owner}:{owner}")
        machine.succeed(f"test $(stat -c %a {shlex.quote(root + '/' + config_name)}) = 600")
        machine.succeed("systemctl restart stravia.service")
        machine.wait_until_succeeds(f"curl -fsS http://127.0.0.1:{port}/readyz")
        machine.succeed(f"python3 ${consumer} persisted {port}")
        machine.succeed("systemctl stop stravia.service")
        machine.succeed("test $(systemctl show stravia.service -p ExecMainStatus --value) = 0")
    # 重启 custom 以从其他 VM 验证实际防火墙与双栈监听，未改变默认实例的 loopback。
    custom.succeed("systemctl start stravia.service")
    custom.wait_until_succeeds("curl -fsS 'http://[::1]:24567/readyz'")
    custom.succeed("python3 ${consumer} persisted 24567 '[::1]'")
    custom.succeed("python3 ${consumer} persisted 8443 localhost /run/stravia-test-pki/ca.pem")
    default.succeed("curl -fsS http://custom:24567/healthz")
    custom.fail("curl -fsS --connect-timeout 3 http://default:23471/healthz")
    default.succeed("systemctl reset-failed stravia.service")
    default.succeed("printf '%s\\n' HOME=/tmp/injected >> '/run/stravia env %$.env'")
    default.fail("systemctl start stravia.service")
    default.succeed("test $(systemctl show stravia.service -p Result --value) = exit-code")
    custom.succeed("systemctl stop stravia.service")
    custom.succeed("systemctl reset-failed stravia.service")
    custom.succeed("install -d -m0700 /var/lib/stravia-sentinel; mv '/srv/stravia %$ state' /srv/stravia-original; chown operator:operator /srv")
    custom.succeed("runuser -u operator -- ln -s /var/lib/stravia-sentinel '/srv/stravia %$ state'")
    custom.fail("systemctl start stravia.service")
    # tmpfiles 对不可信链接可能报错或跳过；关键不变量是 root 目标不被改属主或写入。
    custom.execute("systemd-tmpfiles --create --prefix='/srv/stravia %$ state'")
    custom.succeed("test $(stat -c '%U:%G:%a' /var/lib/stravia-sentinel) = root:root:700; test ! -e /var/lib/stravia-sentinel/custom.toml")
    custom.succeed("systemctl stop stravia.service; rm -- '/srv/stravia %$ state'; mv /srv/stravia-original '/srv/stravia %$ state'; chown root:root /srv; systemctl reset-failed stravia.service")
    # 替代路径本身可运行，失败必须来自选项所有权冲突，而非找不到浏览器。
    custom.succeed("printf '%s\\n' STRAVIA_CHROME_PATH=${alternateBrowser} > /run/stravia-browser.env")
    custom.fail("systemctl start stravia.service")
    custom.succeed("test $(systemctl show stravia.service -p Result --value) = exit-code")
    missing.wait_until_succeeds("systemctl is-failed stravia.service")
    missing.fail("curl -fsS http://127.0.0.1:23471/healthz")
    missing.succeed("test ! -e /var/lib/stravia/server.toml")
  '';
}
