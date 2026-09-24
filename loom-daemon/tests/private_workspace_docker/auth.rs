use super::*;

// Synthetic credentials only; provided to the server at run time, not baked
// into the fixture image. Real host authentication is removed by command().
pub(super) const USERNAME: &str = "fixture-user";
pub(super) const PASSWORD: &str = "synthetic-test-password";
pub(super) const GH_TOKEN: &str = "synthetic-github-token";
pub(super) const SERVER: &str = r#"import base64, http.server, os, ssl
class Forge(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):
        kind = self.path.split('/')[1]
        expected = os.environ.get('FIXTURE_' + kind.upper() + '_AUTH', '')
        authorization = 'Basic ' + base64.b64encode(expected.encode()).decode()
        if not expected or self.headers.get('Authorization') != authorization:
            self.send_response(401)
            self.send_header('WWW-Authenticate', 'Basic realm="fixture"')
            self.end_headers()
            return
        self.path = self.path.removeprefix('/' + kind)
        super().do_GET()
    def log_message(self, *args):
        pass
s=http.server.HTTPServer(('0.0.0.0',8443),Forge)
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
