#!/usr/bin/env python3
"""Local Linux clipboard and save-dialog helper for gitrun manage."""
from __future__ import annotations
import argparse, hmac, os, re, secrets, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path

MAX_LOG_BYTES = 32 * 1024 * 1024
PROTOCOL_TIMEOUT = 600
TOKEN_PREFIX = "/tmp/GitRun-logs-token-"
SOURCE_RE = re.compile(r"^(?P<user>[A-Za-z0-9._-]+)@(?P<host>[A-Za-z0-9.-]+):(?P<path>/tmp/GitRun-logs-[a-f0-9]{32}\.txt)$")
TARGET_RE = re.compile(r"^[A-Za-z0-9._@:-]+$")

def read_line(sock: socket.socket, limit: int = 512) -> str:
    data = bytearray()
    while True:
        chunk = sock.recv(1)
        if not chunk:
            raise ConnectionError("connection closed while reading GitRun protocol")
        if chunk == b"\n":
            return data.decode("ascii")
        if chunk != b"\r":
            if len(data) >= limit:
                raise ValueError("protocol line is too long")
            data.extend(chunk)

def write_line(sock: socket.socket, value: str) -> None:
    sock.sendall(value.encode("ascii") + b"\n")

def read_exact(sock: socket.socket, length: int) -> bytes:
    if length < 0 or length > MAX_LOG_BYTES:
        raise ValueError("log payload exceeds the 32 MiB transfer limit")
    data = bytearray()
    while len(data) < length:
        part = sock.recv(min(65536, length - len(data)))
        if not part:
            raise ConnectionError("connection closed while receiving logs")
        data.extend(part)
    return bytes(data)

def has_desktop() -> bool:
    return bool(os.environ.get("DISPLAY") or os.environ.get("WAYLAND_DISPLAY"))

def copy_to_clipboard(payload: bytes) -> tuple[bool, str]:
    candidates = []
    if os.environ.get("WAYLAND_DISPLAY"):
        candidates.append(("wl-copy", ["wl-copy"]))
    if os.environ.get("DISPLAY"):
        candidates.extend([
            ("xclip", ["xclip", "-selection", "clipboard", "-in"]),
            ("xsel", ["xsel", "--clipboard", "--input"]),
        ])
    reasons = []
    for name, command in candidates:
        executable = shutil.which(name)
        if not executable:
            reasons.append(f"{name} unavailable")
            continue
        try:
            result = subprocess.run([executable, *command[1:]], input=payload,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                                    timeout=2, check=False)
            if result.returncode == 0:
                return True, name
            details = result.stderr.decode("utf-8", "replace").strip()
            reasons.append(f"{name} exited {result.returncode}" + (f": {details[:100]}" if details else ""))
        except subprocess.TimeoutExpired:
            reasons.append(f"{name} timed out after 2 seconds")
        except OSError as error:
            reasons.append(f"{name}: {error}")
    return False, "; ".join(reasons) if reasons else "no local desktop clipboard helper"

def save_dialog(default_name: str) -> tuple[Path | None, bool]:
    if has_desktop():
        zenity = shutil.which("zenity")
        if zenity:
            proposed = str(Path.home() / "Downloads" / default_name)
            try:
                result = subprocess.run([zenity, "--file-selection", "--save", "--confirm-overwrite",
                    "--title=Save GitRun logs", f"--filename={proposed}"],
                    stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
                    timeout=PROTOCOL_TIMEOUT, check=False)
                return (Path(result.stdout.strip()), False) if result.returncode == 0 and result.stdout.strip() else (None, True)
            except (OSError, subprocess.TimeoutExpired):
                return None, True
        kdialog = shutil.which("kdialog")
        if kdialog:
            proposed = str(Path.home() / "Downloads" / default_name)
            try:
                result = subprocess.run([kdialog, "--getsavefilename", proposed, "*.txt|Text files",
                    "--title", "Save GitRun logs"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    text=True, timeout=PROTOCOL_TIMEOUT, check=False)
                return (Path(result.stdout.strip()), False) if result.returncode == 0 and result.stdout.strip() else (None, True)
            except (OSError, subprocess.TimeoutExpired):
                return None, True
        try:
            import tkinter as tk
            from tkinter import filedialog
            root = tk.Tk()
            root.withdraw()
            try:
                selected = filedialog.asksaveasfilename(title="Save GitRun logs",
                    initialdir=str(Path.home() / "Downloads"), initialfile=default_name,
                    defaultextension=".txt", filetypes=[("Text files", "*.txt"), ("All files", "*.*")])
            finally:
                root.destroy()
            return (Path(selected), False) if selected else (None, True)
        except Exception:
            pass
    downloads = Path.home() / "Downloads"
    try:
        downloads.mkdir(mode=0o700, parents=True, exist_ok=True)
        return downloads / default_name, False
    except OSError:
        return Path.home() / default_name, False

def open_default_editor(path: Path) -> None:
    if not has_desktop():
        return
    opener = shutil.which("xdg-open")
    command = [opener, str(path)] if opener else None
    if command is None:
        gio = shutil.which("gio")
        command = [gio, "open", str(path)] if gio else None
    if command:
        try:
            subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        except OSError:
            pass

def save_payload(payload: bytes) -> tuple[str, Path | None]:
    default_name = "GitRun-logs-" + time.strftime("%Y%m%d-%H%M%S") + ".txt"
    path, cancelled = save_dialog(default_name)
    if cancelled or path is None:
        return "CANCELLED", None
    path = path.expanduser()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "wb") as output:
            output.write(payload)
            output.flush()
            os.fsync(output.fileno())
    except OSError as error:
        print(f"Could not save GitRun logs at {path}: {error}", file=sys.stderr)
        return "CANCELLED", None
    open_default_editor(path)
    return "SAVED", path

def handle_client(client: socket.socket, token: str, saved_paths: list[Path]) -> None:
    with client:
        client.settimeout(5)
        try:
            hello = read_line(client)
            if not hmac.compare_digest(hello, "GITRUN_LOGS_V1 " + token):
                write_line(client, "DENIED")
                return
            write_line(client, "READY")
            header = read_line(client)
            match = re.fullmatch(r"LOGS ([0-9]{1,8})", header)
            if not match:
                write_line(client, "ERROR invalid log header")
                return
            data = read_exact(client, int(match.group(1)))
            copied, reason = copy_to_clipboard(data)
            if copied:
                write_line(client, "COPIED")
                return
            write_line(client, "FALLBACK")
            client.settimeout(PROTOCOL_TIMEOUT)
            fallback_header = read_line(client)
            match = re.fullmatch(r"LOGS ([0-9]{1,8})", fallback_header)
            if not match:
                write_line(client, "ERROR invalid fallback header")
                return
            fallback_data = read_exact(client, int(match.group(1)))
            result, path = save_payload(fallback_data)
            if result == "SAVED" and path is not None:
                saved_paths.append(path)
            write_line(client, result)
            if reason:
                print(f"GitRun native clipboard unavailable: {reason}", file=sys.stderr)
        except (OSError, ValueError, ConnectionError) as error:
            try:
                write_line(client, "ERROR local export failed")
            except OSError:
                pass
            print(f"GitRun Logs helper: {error}", file=sys.stderr)

def remote_token(ssh: str, target: str, token: str) -> str:
    path = f"{TOKEN_PREFIX}{token}.txt"
    command = f"umask 077; cat > {path} && chmod 600 {path}"
    result = subprocess.run([ssh, "-T", target, command], input=(token + "\n").encode("ascii"),
                            timeout=30, check=False)
    if result.returncode:
        raise RuntimeError(f"could not create private token file (SSH exit {result.returncode})")
    return path

def run_target(target: str) -> int:
    if not TARGET_RE.fullmatch(target) or target.startswith("-"):
        raise ValueError("target must be user@MainNode or a Tailscale address")
    ssh = shutil.which("ssh")
    if not ssh:
        raise RuntimeError("OpenSSH client is required")
    token = secrets.token_hex(16)
    token_path = remote_token(ssh, target, token)
    remote_port = secrets.randbelow(16000) + 41000
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("127.0.0.1", 0))
    listener.listen(4)
    listener.settimeout(0.1)
    local_port = listener.getsockname()[1]
    forward = f"127.0.0.1:{remote_port}:127.0.0.1:{local_port}"
    command_remote = (f"trap 'rm -f {token_path}' EXIT; GITRUN_LOG_EXPORT_PORT={remote_port} "
                      f"GITRUN_LOG_EXPORT_TOKEN_FILE={token_path} gitrun manage")
    command = [ssh, "-o", "ExitOnForwardFailure=no", "-R", forward, "-tt", target, command_remote]
    saved: list[Path] = []
    process = None
    try:
        process = subprocess.Popen(command)
        while process.poll() is None:
            try:
                client, _ = listener.accept()
            except socket.timeout:
                continue
            handle_client(client, token, saved)
        return process.wait()
    finally:
        listener.close()
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if saved:
            print("\nGitRun logs saved locally:")
            for path in saved:
                print(f"  {path}")
            print("The file was opened with the default text editor when a graphical session was available.")

def receive_source(source: str) -> int:
    match = SOURCE_RE.fullmatch(source)
    if not match:
        raise ValueError("source must be user@host:/tmp/GitRun-logs-<32 hex characters>.txt")
    scp = shutil.which("scp")
    if not scp:
        raise RuntimeError("OpenSSH scp client is required")
    with tempfile.TemporaryDirectory(prefix="gitrun-logs-") as directory:
        tmp = Path(directory) / "remote-logs.txt"
        remote = f"{match.group('user')}@{match.group('host')}:{match.group('path')}"
        result = subprocess.run([scp, remote, str(tmp)], check=False)
        if result.returncode:
            return result.returncode
        status, path = save_payload(tmp.read_bytes())
        if status == "SAVED" and path is not None:
            print(f"Saved GitRun logs to {path}")
            return 0
        print("Save cancelled; no local file was written.")
        return 1

def main() -> int:
    parser = argparse.ArgumentParser(description="Run gitrun manage with Linux clipboard and log-export fallback.")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--target", help="SSH target, such as user@MainNode")
    group.add_argument("--source", help="download an existing remote .txt fallback")
    args = parser.parse_args()
    try:
        return run_target(args.target) if args.target else receive_source(args.source)
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"GitRun Logs helper: {error}", file=sys.stderr)
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
