#!/usr/bin/env python3
"""Linux session-image smoke: start a disposable Claude seat, dispatch, scan and stop."""
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import tempfile


def scan_archive(command, token):
    # docker export omits bind mounts; callers also stream each mounted tree.
    # An archive read failure is an inconclusive scan and fails this test.
    with subprocess.Popen(command, stdout=subprocess.PIPE) as process:
        overlap = b""
        for block in iter(lambda: process.stdout.read(1024 * 1024), b""):
            data = overlap + block
            if token in data:
                raise AssertionError("fixture token persisted on the seat filesystem")
            overlap = data[-(len(token) - 1):]
        if process.wait():
            raise RuntimeError("could not scan the seat filesystem")


def smoke(image, scratch):
    scripts = Path(__file__).resolve().parents[2] / "defaults/scripts"
    workspace = scratch / "repo"
    shutil.copytree(scripts, workspace / ".loom/scripts", symlinks=True)
    subprocess.run(["git", "init", "-q", workspace], check=True)
    daemon = scratch / "loom-daemon"
    with daemon.open("wb") as out:
        subprocess.run(["docker", "run", "--rm", "--entrypoint", "cat", image,
                        "/usr/local/bin/loom-daemon"], stdout=out, check=True)
    daemon.chmod(0o700)

    name = "ci-" + secrets.token_hex(8)
    token = ("sk-ant-oat01-fixture-" + secrets.token_hex(32)).encode()
    pool, seats, home = (scratch / p for p in ("tokens", "seats", "operator"))
    for directory in (pool, seats, home):
        directory.mkdir(mode=0o700)
    file = os.open(pool / (name + ".token"), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
    with os.fdopen(file, "wb") as out:
        out.write(token)
    # Only this fixture's credential and workspace are visible to the adapter.
    env = dict(PATH=os.environ["PATH"], HOME=str(home), LOOM_WORKSPACE=str(workspace),
               LOOM_DAEMON_BIN=str(daemon), LOOM_DAEMON_SELF_BIN=str(daemon),
               LOOM_CLAUDE_SEAT_ROOT=str(seats), LOOM_SHARED_TOKENS_DIR=str(pool),
               LOOM_SWEEP_CPU_QUOTA="0", LOOM_SWEEP_NICE="0")

    def lifecycle(action, *args):
        return json.loads(subprocess.check_output(
            [daemon, "accounts", "--workspace", workspace, "session", action, name,
             "--provider", "claude", "--json", *args], cwd=workspace, env=env))

    seat = lifecycle("start", "--image", image)
    container = seat["container_id"]
    seat_home = seats / name / "home"
    # Hosted Linux runners and the image can use different uids. Adjust only
    # this synthetic fixture's bind source, restoring it for normal cleanup.
    def own_fixture(owner):
        subprocess.run(["docker", "run", "--rm", "--user", "0", "-v", f"{seat_home}:/fixture",
                        "--entrypoint", "chown", image, "-R", owner, "/fixture"], check=True)

    changed_owner = False
    try:
        uid = subprocess.check_output(["docker", "exec", container, "id", "-u"], text=True).strip()
        if uid != str(os.getuid()):
            own_fixture(uid)
            changed_owner = True
        result = subprocess.run([workspace / ".loom/scripts/spawn-claude.sh", "auth", "status", "--json"],
                                cwd=workspace, env=env, capture_output=True, check=True)
        assert json.loads(result.stdout)["loggedIn"] is True
        assert f"containment=claude-session container_id={container} image_digest=sha256:".encode() in result.stderr
        scan_archive(["docker", "export", container], token)
        mounts = json.loads(subprocess.check_output(["docker", "inspect", container]))[0]["Mounts"]
        for mount in mounts:
            scan_archive(["docker", "cp", f"{container}:{mount['Destination']}", "-"], token)
    finally:
        try:
            stopped = lifecycle("stop", "--force")
            assert not stopped["running"] and stopped["container_id"] is None
        finally:
            if changed_owner:
                own_fixture(str(os.getuid()))
    print("PASS real Claude seat start, auth dispatch, filesystem scan and stop")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as directory:
        smoke(sys.argv[1], Path(directory))
