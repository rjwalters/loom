use super::*;

// Synthetic credentials only; provided to the server at run time, not baked
// into the fixture image. Real host authentication is removed by command().
pub(super) const USERNAME: &str = "fixture-user";
pub(super) const PASSWORD: &str = "synthetic-test-password";
pub(super) const GH_TOKEN: &str = "synthetic-github-token";
pub(super) const SERVER: &str = r#"import base64, http.server, os, ssl, subprocess, urllib.parse, json
class Forge(http.server.SimpleHTTPRequestHandler):
    protocol_version="HTTP/1.1"
    def dispatch(self):
        if self.path == '/fixture/pr' and self.command == 'POST':
            body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))))
            with open('/srv/pr.json','w') as f: json.dump(body,f)
            response=b'{"url":"https://fixture.invalid/fixture/repo/pull/17"}'; self.send_response(200); self.send_header('Content-Length',str(len(response))); self.end_headers(); self.wfile.write(response); return
        kind = self.path.split('/')[1]
        expected = os.environ.get('FIXTURE_' + kind.upper() + '_AUTH', '')
        authorization = 'Basic ' + base64.b64encode(expected.encode()).decode()
        if not expected or self.headers.get('Authorization') != authorization:
            self.send_response(401); self.send_header('WWW-Authenticate', 'Basic realm="fixture"'); self.send_header('Content-Length','0'); self.end_headers(); return
        parsed=urllib.parse.urlsplit(self.path.removeprefix('/'+kind))
        env=dict(os.environ, GIT_PROJECT_ROOT='/srv', GIT_HTTP_EXPORT_ALL='1', PATH_INFO=parsed.path, QUERY_STRING=parsed.query, REQUEST_METHOD=self.command, CONTENT_TYPE=self.headers.get('Content-Type',''), REMOTE_USER='fixture')
        data=self.rfile.read(int(self.headers.get('Content-Length','0')))
        result=subprocess.run(['git','http-backend'],input=data,stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
        if result.stderr: print(result.stderr.decode(),flush=True)
        headers,body=result.stdout.split(b'\r\n\r\n',1)
        parsed_headers=[line.decode().split(':',1) for line in headers.split(b'\r\n')]
        status=next((int(value.strip().split()[0]) for key,value in parsed_headers if key.lower()=='status'),200)
        self.send_response(status)
        for line in headers.split(b'\r\n'):
            key,value=line.decode().split(':',1)
            if key.lower()!='status': self.send_header(key,value.strip())
        self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
    do_GET=dispatch
    do_POST=dispatch
    def log_message(self,*args): pass
s=http.server.ThreadingHTTPServer(('0.0.0.0',8443),Forge)
c=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
c.load_cert_chain('/srv/cert.pem','/srv/key.pem')
s.socket=c.wrap_socket(s.socket,server_side=True)
s.serve_forever()
"#;

#[test]
#[ignore = "requires Docker; explicitly run by CI"]
fn authenticated_https_clone_and_fetch_preserve_external_forge_context() {
    let f = Fixture::new();
    let gitea = &f.names[0];
    f.start(gitea); // Initial preparation must forward username and password.
    checked(f.job(gitea, "role", "git fetch origin").output().unwrap());
    checked(
        f.job(gitea, "sweep", "git fetch origin")
            .env_remove("GITEA_TOKEN")
            .env("FORGE_TOKEN", PASSWORD)
            .output()
            .unwrap(),
    );
    let wrong = f
        .job(gitea, "role", "git fetch origin")
        .env("GITEA_USERNAME", "wrong-user")
        .output()
        .unwrap();
    assert!(!wrong.status.success(), "fixture must require the configured Basic username");
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("workspace Git fetch failed"));
    assert!(!String::from_utf8_lossy(&wrong.stderr).contains(PASSWORD));

    let github = &f.names[1];
    let remote = f.repository.replace("/gitea/", "/github/");
    let host = reqwest::Url::parse(&remote)
        .unwrap()
        .host_str()
        .unwrap()
        .to_owned();
    checked(
        f.command(&[
            "session",
            "start",
            github,
            "--private-clone",
            &remote,
            "--image",
            &f.image,
        ])
        .env("GH_HOST", &host)
        .env("GH_TOKEN", GH_TOKEN)
        .output()
        .unwrap(),
    );
    checked(
        f.job(github, "interactive", "git fetch origin")
            .env("GH_HOST", &host)
            .env("GH_TOKEN", GH_TOKEN)
            .output()
            .unwrap(),
    );
    checked(
        f.job(github, "role", "git fetch origin")
            .env("GH_HOST", &host)
            .env("GITHUB_TOKEN", GH_TOKEN)
            .output()
            .unwrap(),
    );
    let missing_host = f
        .job(github, "role", "git fetch origin")
        .env("GH_TOKEN", GH_TOKEN)
        .output()
        .unwrap();
    assert!(
        !missing_host.status.success(),
        "fixture must require non-default GH_HOST selection"
    );
    for name in [gitea, github] {
        let config = f.exec(name, "cat /workspace/repo/.git/config");
        for credential in [USERNAME, PASSWORD, GH_TOKEN] {
            assert!(!config.contains(credential), "credential stored in repository config");
        }
    }
}
