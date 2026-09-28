#!/usr/bin/env python3
"""Real Conan 2 upload -> scheduled build -> upload -> fresh-cache consumer.

Requires conan, cmake, a native compiler, and a built conan-server binary.
No dependency downloads or personal Conan cache are used.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import secrets
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "debug" / ("conan-server.exe" if os.name == "nt" else "conan-server"))
    parser.add_argument("--ios-sdk", choices=["iphoneos", "iphonesimulator"])
    parser.add_argument("--with-channel", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve()
    native_os = {"Darwin": "Macos", "Windows": "Windows", "Linux": "Linux"}[platform.system()]
    architecture = "armv8" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
    if args.ios_sdk == "iphoneos":
        architecture = "armv8"
    recipe_ref = "forge-hello/0.1" + ("@demo/testing" if args.with_channel else "")
    assert not args.ios_sdk or native_os == "Macos", "iOS requires a Mac worker"
    target = {"id": "smoke", "runner_os": native_os, "os": "iOS" if args.ios_sdk else native_os, "arch": architecture, "build_type": "Release"}
    if args.ios_sdk:
        target.update(sdk=args.ios_sdk, os_version="15.0")
    other_os = "Linux" if native_os != "Linux" else "Windows"
    other = {"id": "other-platform", "runner_os": other_os, "os": other_os, "arch": "x86_64", "build_type": "Release"}
    publish_token, worker_token = secrets.token_hex(32), secrets.token_hex(32)
    with tempfile.TemporaryDirectory(prefix="conan-server-smoke-") as directory:
        work = Path(directory)
        matrix = work / "targets.json"
        matrix.write_text(json.dumps([target, other]))
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        server_env = dict(os.environ, CONAN_SERVER_PUBLISH_TOKEN=publish_token, CONAN_SERVER_WORKER_TOKEN=worker_token)
        server_args = [str(binary), "serve", "--listen", f"127.0.0.1:{port}", "--data", str(work / "data"), "--targets", str(matrix)]
        server_log = (work / "server.log").open("wb")
        server = subprocess.Popen(server_args, env=server_env, stdout=server_log, stderr=subprocess.STDOUT)

        def request(path, method="GET", data=None, token=None, expected=200, raw=False):
            headers = {}
            if token:
                headers["Authorization"] = f"Bearer {token}"
            if data is not None and not isinstance(data, bytes):
                data = json.dumps(data).encode()
                headers["Content-Type"] = "application/json"
            req = urllib.request.Request(url + path, data=data, method=method, headers=headers)
            try:
                response = urllib.request.urlopen(req, timeout=15)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                content = response.read()
                assert response.status == expected, (path, response.status, content)
                return content if raw or not content else json.loads(content)

        def run(command, home=None, authenticate=False):
            env = dict(os.environ, CONAN_NON_INTERACTIVE="1")
            env.pop("CONAN_SERVER_PUBLISH_TOKEN", None)
            env.pop("CONAN_SERVER_WORKER_TOKEN", None)
            if home:
                env["CONAN_HOME"] = str(work / home)
            if authenticate:
                env["CONAN_LOGIN_USERNAME_FORGE"] = "publisher"
                env["CONAN_PASSWORD_FORGE"] = publish_token
            if command[0] == str(binary):
                env["CONAN_SERVER_WORKER_TOKEN"] = worker_token
            result = subprocess.run(command, cwd=work, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=240)
            if result.returncode:
                output = (result.stdout + result.stderr).replace(publish_token, "[redacted]").replace(worker_token, "[redacted]")
                raise AssertionError(f"Failed: {command}\n{output}")
            return result.stdout

        def setup(home):
            run(["conan", "profile", "detect", "--force"], home)
            run(["conan", "remote", "disable", "conancenter"], home)
            run(["conan", "remote", "add", "forge", url], home)

        try:
            for _ in range(100):
                if server.poll() is not None:
                    raise AssertionError((work / "server.log").read_text())
                try:
                    request("/health")
                    break
                except urllib.error.URLError:
                    time.sleep(0.1)
            else:
                raise AssertionError("server did not become healthy")
            request("/api/jobs", expected=401, raw=True)
            request("/api/jobs/claim", "POST", {"worker_id":"test", "runner_os":native_os,"target_ids":["smoke"]}, publish_token, expected=403, raw=True)
            prefix = "/v2/conans/incomplete/0.1/_/_/revisions/abc/files/"
            request(prefix + "conanfile.py", "PUT", b"test", expected=401, raw=True)
            manifest = f"1\nconanfile.py: {hashlib.md5(b'test').hexdigest()}\nexport_source/source.c: {'0'*32}\n".encode()
            request(prefix + "conanfile.py", "PUT", b"test", publish_token, expected=201, raw=True)
            request(prefix + "conanmanifest.txt", "PUT", manifest, publish_token, expected=201, raw=True)
            request(prefix.rstrip('/'), expected=404, raw=True)
            assert request("/api/jobs", token=publish_token) == [], "incomplete recipes must not enqueue builds"
            request(prefix + "conanfile.py", "PUT", b"changed", publish_token, expected=409, raw=True)
            setup("publisher")
            export_args = ["--user=demo", "--channel=testing"] if args.with_channel else []
            run(["conan", "export", str(ROOT / "tests/fixtures/hello"), *export_args], "publisher")
            run(["conan", "upload", recipe_ref, "-r=forge", "--only-recipe", "--confirm", "--check"], "publisher", True)
            jobs = request("/api/jobs", token=publish_token)
            assert len(jobs) == 2 and all(j["status"] == "queued" for j in jobs), jobs
            run(["conan", "upload", recipe_ref, "-r=forge", "--only-recipe", "--confirm"], "publisher", True)
            assert len(request("/api/jobs", token=publish_token)) == 2, "duplicate upload enqueued another job"
            reference = jobs[0]["reference"]
            # A native worker must not receive jobs for another operating system.
            assert request("/api/jobs/claim", "POST", {"worker_id":"wrong-os","runner_os":other_os,"target_ids":["smoke"]}, worker_token) is None
            run([str(binary), "worker", "--server", url, "--id", "smoke-worker", "--target", "smoke", "--work", str(work / "worker"), "--once", "--no-conancenter"])
            jobs = request("/api/jobs", token=publish_token)
            built = next(j for j in jobs if j["target"]["id"] == "smoke")
            assert built["status"] == "succeeded", jobs
            assert next(j for j in jobs if j["target"]["id"] == "other-platform")["status"] == "queued"
            assert not list((work / "worker").glob("*/conan-home")), "worker credentials/cache were retained"
            # Restart and verify both durable jobs and package storage.
            server.terminate()
            server.wait(timeout=10)
            server = subprocess.Popen(server_args, env=server_env, stdout=server_log, stderr=subprocess.STDOUT)
            for _ in range(100):
                try:
                    request("/health")
                    break
                except urllib.error.URLError:
                    time.sleep(0.1)
            setup("consumer")
            settings = [f"-s:h=os={target['os']}", f"-s:h=arch={target['arch']}", "-s:h=build_type=Release"]
            if args.ios_sdk:
                settings += [f"-s:h=os.sdk={args.ios_sdk}", "-s:h=os.version=15.0"]
            output = run(["conan", "install", f"--requires={reference}", "--build=never", *settings, "--format=json", "--output-folder=consumer-output"], "consumer")
            graph = json.loads(output)["graph"]["nodes"]
            package = next(node for node in graph.values() if str(node.get("ref", "")).startswith("forge-hello/"))
            assert package["binary"] == "Download", package
            package_folder = Path(package["package_folder"])
            assert (package_folder / "include/hello.h").is_file()
            assert list((package_folder / "lib").glob("*forge_hello*"))
            listing = json.loads(run(["conan", "list", "forge-hello/*#*:*#*", "-r=forge", "--format=json"], "consumer"))
            assert recipe_ref in listing["forge"], listing
            # Native consumer actually links and runs the downloaded library.
            if not args.ios_sdk:
                consumer = work / "consumer-source"
                consumer.mkdir()
                (consumer / "main.c").write_text('#include "hello.h"\nint main(void) { return forge_hello() == 42 ? 0 : 1; }\n')
                (consumer / "CMakeLists.txt").write_text('cmake_minimum_required(VERSION 3.20)\nproject(consumer C)\nadd_executable(consumer main.c)\ntarget_include_directories(consumer PRIVATE "${PACKAGE_FOLDER}/include")\nfind_library(HELLO_LIBRARY NAMES forge_hello PATHS "${PACKAGE_FOLDER}/lib" NO_DEFAULT_PATH REQUIRED)\ntarget_link_libraries(consumer PRIVATE ${HELLO_LIBRARY})\n')
                build = work / "consumer-build"
                run(["cmake", "-S", str(consumer), "-B", str(build), f"-DPACKAGE_FOLDER={package_folder.as_posix()}"])
                run(["cmake", "--build", str(build), "--config", "Release"])
                executable = build / "consumer"
                if os.name == "nt":
                    executable = build / "Release/consumer.exe"
                    if not executable.exists():
                        executable = build / "consumer.exe"
                run([str(executable)])
            print(f"PASS {target['os']} {architecture} {args.ios_sdk or 'native'}: authenticated upload, incomplete-upload gate, deduplication, OS routing, automatic build, restart, fresh-cache download" + (", linked consumer" if not args.ios_sdk else ""))
        except BaseException:
            print((work / "server.log").read_text(errors="replace")[-12000:])
            raise
        finally:
            server.terminate()
            server.wait(timeout=10)
            server_log.close()


if __name__ == "__main__":
    main()
