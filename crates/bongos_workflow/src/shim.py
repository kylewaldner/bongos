# The bongos program shim: `python -I -B shim.py <policy.json> <script>`.
#
# It reads the node's permissions from the policy file, installs an audit hook
# (sys.addaudithook) that denies what the node did not declare, then runs the
# script with runpy as __main__. A denial raises
# PermissionError("permission denied: <what>") and writes one JSON line to the
# denials pipe (file descriptor 3), which bongos reads after the script exits;
# any denial fails the node even when the script caught the exception.
#
# Denied: starting a program not in `commands` (subprocess.Popen, os.system,
# os.exec*, os.posix_spawn*, matched by basename after a PATH lookup), opening
# or changing a file outside the writable paths, and connecting a socket when
# the node declares no network (or to a host it did not declare). Reads are
# never blocked. This is the second layer; the OS sandbox is the first.

import sys


def _install(policy):
    import json
    import os
    import shlex
    import shutil
    import socket
    import threading

    commands = set(policy.get("commands") or [])
    hosts = [host.lower().rstrip(".") for host in policy.get("network") or []]
    writable = []
    for path in policy.get("writable") or []:
        writable.append(os.path.realpath(path))
    denials_fd = policy.get("denials_fd", 3)
    shells = {"sh", "bash", "dash", "zsh", "ksh"}
    shell_syntax = set(";&|`$()<>\n\\*?{}[]~")

    allowed_ips = set()
    proxies = set()
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        value = os.environ.get(name)
        if not value:
            continue
        rest = value.split("://", 1)[-1].split("/", 1)[0].rsplit("@", 1)[-1]
        host, _, port = rest.rpartition(":")
        if host and port.isdigit():
            proxies.add((host.strip("[]"), int(port)))

    local = threading.local()

    def deny(what):
        line = json.dumps({"what": what}) + "\n"
        try:
            os.write(denials_fd, line.encode("utf-8"))
        except OSError:
            pass
        raise PermissionError("permission denied: " + what)

    # Writes.

    def resolve(path):
        if isinstance(path, int) or path is None:
            return None
        try:
            path = os.fspath(path)
        except TypeError:
            return None
        if isinstance(path, bytes):
            path = os.fsdecode(path)
        return os.path.realpath(os.path.join(os.getcwd(), path))

    def is_writable(path):
        if path is None:
            return True
        if path == "/dev/null" or path.startswith("/dev/fd/") or path.startswith("/proc/self/fd/"):
            return True
        for root in writable:
            if path == root or path.startswith(root.rstrip(os.sep) + os.sep):
                return True
        return False

    def check_write(path):
        if path in ("/dev/stdout", "/dev/stderr", b"/dev/stdout", b"/dev/stderr"):
            return
        resolved = resolve(path)
        if not is_writable(resolved):
            deny("write " + resolved)

    write_flags = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC

    def on_open(args):
        path, mode, flags = (list(args) + [None, None, None])[:3]
        writes = False
        if isinstance(mode, str) and any(letter in mode for letter in "wax+"):
            writes = True
        if isinstance(flags, int) and flags & write_flags:
            writes = True
        if writes:
            check_write(path)

    # Programs.

    def program_name(program):
        program = os.fsdecode(program) if isinstance(program, bytes) else str(program)
        found = shutil.which(program) if os.sep not in program else program
        return os.path.basename(found or program)

    def check_shell_line(line):
        if any(char in shell_syntax for char in line):
            deny("shell command: " + line.strip()[:80])
        try:
            words = shlex.split(line)
        except ValueError:
            deny("shell command: " + line.strip()[:80])
        if words:
            check_argv(words[0], words)

    def check_argv(program, argv):
        argv = [os.fsdecode(arg) if isinstance(arg, bytes) else str(arg) for arg in argv or []]
        if program is None:
            if not argv:
                return
            program = argv[0]
        name = program_name(program)
        if name in commands:
            return
        if name in shells and "-c" in argv:
            index = argv.index("-c")
            if index + 1 < len(argv):
                check_shell_line(argv[index + 1])
                return
        deny(name)

    def on_popen(args):
        executable, argv = args[0], args[1]
        if isinstance(argv, (str, bytes)):
            argv = shlex.split(os.fsdecode(argv) if isinstance(argv, bytes) else argv)
        check_argv(executable, list(argv or []))

    def on_system(args):
        command = args[0]
        if isinstance(command, bytes):
            command = os.fsdecode(command)
        check_shell_line(str(command))

    def on_exec(args):
        check_argv(args[0], list(args[1] or []))

    def on_spawn(args):
        check_argv(args[1], list(args[2] or []))

    # Network.

    def host_allowed(host):
        host = host.lower().rstrip(".")
        for allowed in hosts:
            if host == allowed:
                return True
            if allowed.startswith("*.") and host.endswith(allowed[1:]):
                return True
        return False

    def on_getaddrinfo(args):
        host = args[0]
        if isinstance(host, bytes):
            host = host.decode("ascii", "replace")
        if not hosts or not isinstance(host, str) or not host_allowed(host):
            return
        try:
            for info in socket.getaddrinfo(host, None):
                allowed_ips.add(info[4][0])
        except OSError:
            pass

    def check_address(sock, address):
        if address is None:
            return
        if isinstance(address, tuple) and len(address) >= 2:
            host, port = str(address[0]), address[1]
            where = "%s:%s" % (host, port)
            if hosts and ((host, port) in proxies or host in allowed_ips or host_allowed(host)):
                return
        else:
            where = os.fsdecode(address) if isinstance(address, bytes) else str(address)
        deny("network " + where)

    def on_connect(args):
        check_address(args[0], args[1])

    handlers = {
        "open": on_open,
        "os.remove": lambda args: check_write(args[0]),
        "os.rmdir": lambda args: check_write(args[0]),
        "os.mkdir": lambda args: check_write(args[0]),
        "os.rename": lambda args: (check_write(args[0]), check_write(args[1])),
        "os.link": lambda args: check_write(args[1]),
        "os.symlink": lambda args: check_write(args[1]),
        "os.truncate": lambda args: check_write(args[0]),
        "os.chmod": lambda args: check_write(args[0]),
        "os.chown": lambda args: check_write(args[0]),
        "os.utime": lambda args: check_write(args[0]),
        "subprocess.Popen": on_popen,
        "os.system": on_system,
        "os.exec": on_exec,
        "os.posix_spawn": on_exec,
        "os.spawn": on_spawn,
        "socket.getaddrinfo": on_getaddrinfo,
        "socket.connect": on_connect,
        "socket.sendto": on_connect,
        "socket.sendmsg": on_connect,
    }

    def hook(event, args):
        handler = handlers.get(event)
        if handler is None or getattr(local, "busy", False):
            return
        local.busy = True
        try:
            handler(args)
        finally:
            local.busy = False

    sys.addaudithook(hook)


def _main():
    import json
    import runpy

    if len(sys.argv) < 3:
        sys.stderr.write("usage: shim.py <policy.json> <script>\n")
        sys.exit(2)
    policy_path, script = sys.argv[1], sys.argv[2]
    with open(policy_path, "r", encoding="utf-8") as policy_file:
        policy = json.load(policy_file)
    _install(policy)
    sys.argv = [script] + sys.argv[3:]
    runpy.run_path(script, run_name="__main__")


if __name__ == "__main__":
    _main()
