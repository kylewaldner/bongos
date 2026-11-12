# The bongos program shim: `python -I -B shim.py <policy.json> <script>`.
#
# It reads the node's permissions from the policy file, installs an audit hook
# (sys.addaudithook) that denies what the node did not declare, then runs the
# script with runpy as __main__. A denial raises
# PermissionError("permission denied: <what>") and writes one JSON line to the
# denials pipe (handed over as file descriptor 3 and moved to a high number at
# startup), which bongos reads after the script exits; any denial fails the
# node even when the script caught the exception. When the script ends, the
# shim writes an end marker to the pipe; a clean exit without it fails the node,
# so closing the pipe cannot hide a denial.
#
# Denied: starting a program not in `commands` (subprocess.Popen, os.system,
# os.exec*, os.posix_spawn*, and the fork_exec under subprocess, replaced by a
# checking wrapper; the program is looked up on the PATH the call uses and its
# real path must be the one a listed command had at startup, and nothing may come
# from or resolve into a writable path; a shell only as `<shell> -c <line>` whose
# line names a listed command; an environment that makes a shell, the dynamic
# loader or Python run other code, set for a program or in the shim's own),
# opening or changing a file outside the writable paths or inside a protected one
# (the OS sandbox's allowlist: the worktree, the scratch directory, the common git
# directory's objects/, refs/ and logs/ and the worktree's own git directory; its
# .git file, gitdir, commondir and config.worktree protected, and refs/replace,
# objects/info/alternates and objects/info/http-alternates too)
# (relative to a directory descriptor or through a file descriptor too; FIFOs and
# device nodes included), creating any symlink, writing through a symlink that
# sits inside the writable git paths, renaming a symlink or hard linking anything
# into them (a later host git would write through it), connecting a socket when the node declares no network
# (or to a host it did not declare), and the introspection that reaches the hook's
# own state (gc object walks, ctypes, frame walks into the shim, tracing). Reads
# are never blocked. Only plain str and bytes (and their subclasses, read without
# running their code) are accepted as paths and program arguments.
#
# This shim is a guardrail, not a security boundary: Python code in the same
# process can always defeat an audit hook. The OS sandbox is the boundary; a run
# without it ("not sandboxed" in the try's summary) is not hard-confined. Known
# gaps, accepted for that reason: a walk through closures (__closure__,
# cell_contents) reaches the hook's state; os.link checks only the destination
# (and refuses one into the git paths);
# binding a unix socket creates a file anywhere; the arguments and environment of
# a listed command are not checked (git -c core.fsmonitor=... runs).

import sys


def _install(policy):
    import json
    import os
    import shlex
    import shutil
    import socket
    import subprocess  # loaded before the hook, so its own imports are not checked
    import threading
    import types

    fs_encoding = sys.getfilesystemencoding()
    environ = os.environ
    environ_data = getattr(environ, "_data", None)
    # Names that make a shell, the dynamic loader or Python run code the script chose.
    risky_names = frozenset(("BASH_ENV", "ENV", "SHELLOPTS", "BASHOPTS", "PS4", "PROMPT_COMMAND"))
    risky_prefixes = ("LD_", "BASH_FUNC_")

    def norm(path):
        return os.path.realpath(path).rstrip(os.sep) or os.sep

    writable = tuple(norm(path) for path in policy.get("writable") or [])
    protected = tuple(norm(path) for path in policy.get("protected") or [])
    git_roots = tuple(norm(path) for path in policy.get("git") or [])
    hosts = tuple(host.lower().rstrip(".") for host in policy.get("network") or [])
    shell_names = frozenset(("sh", "bash", "dash", "zsh", "ksh"))
    shell_syntax = frozenset(";&|`$()<>\n\\*?{}[]~")
    shim_file = norm(__file__)

    # The denials pipe, moved off 3 so a script that closes or reuses 3 does not lose it.
    # Closed at 3 so the programs it starts do not inherit it.
    handed = policy.get("denials_fd", 3)
    try:
        denials_fd = os.dup(handed)
        moved = fcntl_dup_high(denials_fd)
        if moved is not None:
            os.close(denials_fd)
            denials_fd = moved
        os.close(handed)
    except OSError:
        denials_fd = -1

    def under(path, roots):
        for root in roots:
            if path == root or path.startswith(root.rstrip(os.sep) + os.sep):
                return True
        return False

    def lookup(name, search_path):
        """The real path the OS would run for `name`, or None. Denies a search through a
        relative or writable PATH entry, which the script could fill after the check."""
        for entry in (search_path or "").split(os.pathsep):
            if not entry or not os.path.isabs(entry) or under(norm(entry), writable):
                deny("command from a writable PATH entry: " + name)
            candidate = os.path.join(entry, name)
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                return candidate
        return None

    launch_path = os.environ.get("PATH", os.defpath)
    command_names = frozenset(os.path.basename(command) for command in policy.get("commands") or [])
    allowed_programs = set()
    for command in policy.get("commands") or []:
        found = shutil.which(command, path=launch_path)
        if found and not under(norm(found), writable):
            allowed_programs.add(norm(found))
    shells = set()
    for shell in shell_names:
        found = shutil.which(shell, path=launch_path)
        if found:
            shells.add(norm(found))
    allowed_programs = frozenset(allowed_programs)
    shells = frozenset(shells)

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
    proxies = frozenset(proxies)

    local = threading.local()

    def deny(what):
        line = json.dumps({"what": what}) + "\n"
        try:
            os.write(denials_fd, line.encode("utf-8"))
        except OSError:
            pass
        raise PermissionError("permission denied: " + what)

    def decode(raw):
        return bytes.decode(raw, fs_encoding, "surrogateescape")

    def text(value):
        """`value` as a str without running any of the script's code, or None. Only the type is
        looked at (not `isinstance`, which reads a `__class__` the script may define), and a
        subclass of str or bytes is copied by the built-in method, never its own."""
        kind = type(value)
        if kind is str:
            return value
        if kind is bytes:
            return decode(value)
        if issubclass(kind, str):
            return str.__str__(value)
        if issubclass(kind, bytes):
            return decode(bytes.__getitem__(value, slice(None)))
        return None

    # Writes.

    def fd_path(fd):
        """The file a descriptor names (Linux /proc), or None when unknown."""
        try:
            name = os.readlink("/proc/self/fd/%d" % fd)
        except (OSError, OverflowError):
            return None
        return name if name.startswith(os.sep) and not name.endswith(" (deleted)") else None

    def dir_base(dir_fd):
        """The directory a descriptor names (Linux /proc), or None when unknown."""
        if dir_fd is None or (type(dir_fd) is int and dir_fd in (-1, -100)):  # none, AT_FDCWD
            return os.getcwd()
        if type(dir_fd) is not int:
            return None
        try:
            return os.readlink("/proc/self/fd/%d" % dir_fd)
        except OSError:
            return None

    def resolve(path, dir_fd=None):
        if path is None:
            return None
        if type(path) is int:
            # fchmod, fchown, futimens, ftruncate, fsetxattr: the file the descriptor names.
            name = fd_path(path)
            if name is None:
                deny("change through a file descriptor bongos cannot check")
            return os.path.realpath(name)
        name = text(path)
        if name is None:
            deny("write to a path object bongos cannot check")
        if os.path.isabs(name):
            return os.path.realpath(name)
        base = dir_base(dir_fd)
        if base is None:
            deny("write relative to a directory descriptor: " + name)
        return os.path.realpath(os.path.join(base, name))

    def is_writable(path):
        if path is None:
            return True
        if path == "/dev/null" or path.startswith("/dev/fd/") or path.startswith("/proc/self/fd/"):
            return True
        return under(path, writable) and not under(path, protected)

    def unresolved(path, dir_fd=None):
        """The absolute path `path` names with no symlink resolved, or None (a descriptor)."""
        if path is None or type(path) is int:
            return None
        name = text(path)
        if name is None:
            return None
        if os.path.isabs(name):
            return name
        base = dir_base(dir_fd)
        return None if base is None else os.path.join(base, name)

    def git_symlink(name):
        """The first symlink sitting inside the writable git paths that resolving `name` passes
        (its last part included), or None. A later host git would follow it too."""
        if not git_roots or name is None:
            return None
        current = os.sep
        for part in name.split(os.sep):
            if part in ("", "."):
                continue
            if part == "..":
                current = os.path.dirname(current)
                continue
            candidate = os.path.join(current, part)
            if os.path.islink(candidate):
                if under(candidate, git_roots):
                    return candidate
                current = os.path.realpath(candidate)
            else:
                current = candidate
        return None

    def check_write(path, dir_fd=None):
        if type(path) in (str, bytes) and path in (
            "/dev/stdout",
            "/dev/stderr",
            b"/dev/stdout",
            b"/dev/stderr",
        ):
            return
        resolved = resolve(path, dir_fd)
        if not is_writable(resolved):
            deny("write " + resolved)
        found = git_symlink(unresolved(path, dir_fd))
        if found is not None:
            deny("write through a symlink in git " + found)

    def check_into_git(source, source_dir_fd, destination, destination_dir_fd, what):
        """Denies moving or linking a symlink (or, for a hard link, anything) into the writable
        git paths, where a later host git would write through it."""
        target = resolve(destination, destination_dir_fd)
        if target is None or not under(target, git_roots):
            return
        if what == "link":
            deny("hard link into git " + target)
        name = unresolved(source, source_dir_fd)
        if name is None or os.path.islink(name):
            deny("move a symlink into git " + target)

    def on_rename(args):
        check_write(args[0], args[2])
        check_write(args[1], args[3])
        check_into_git(args[0], args[2], args[1], args[3], "rename")

    def on_link(args):
        check_write(args[1], args[3])
        check_into_git(args[0], args[2], args[1], args[3], "link")

    def on_symlink(args):
        deny("create a symlink " + (text(args[1]) or "bongos cannot check"))

    write_flags = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC

    def on_open(args):
        path, mode, flags = (list(args) + [None, None, None])[:3]
        writes = False
        if type(mode) is str and any(letter in mode for letter in "wax+"):
            writes = True
        if type(flags) is int and flags & write_flags:
            writes = True
        if not writes:
            return
        # os.open reports no mode and not its dir_fd, so a relative path there may name a
        # directory descriptor's file: refuse it rather than guess.
        name = text(path)
        if mode is None and name is not None and not os.path.isabs(name):
            deny("write relative to a directory descriptor: " + name)
        check_write(path)

    # Programs.

    def own_path():
        if type(environ_data) is dict:
            found = dict.get(environ_data, b"PATH")
            if type(found) is bytes:
                return decode(found)
            if found is None:
                return os.defpath
        return launch_path

    def risky(name, python):
        if name in risky_names or name.startswith(risky_prefixes):
            return True
        return python and name.startswith("PYTHON")

    def check_env(env, program):
        """Denies an environment that makes the program run other code."""
        if env is None or env is environ:
            return
        if type(env) is not dict:
            deny("a program started with an environment bongos cannot check")
        python = "python" in os.path.basename(program)
        for name in dict.keys(env):
            name = text(name)
            if name is None:
                deny("a program started with an environment bongos cannot check")
            if risky(name, python):
                deny("a program started with " + name + " set")

    def exec_path(env):
        if env is None or env is environ:
            return own_path()
        if type(env) is not dict:
            deny("a program started with an environment bongos cannot check")
        for name, value in dict.items(env):
            if type(name) in (str, bytes) and text(name) == "PATH":
                found = text(value)
                if found is None:
                    deny("a program started with a PATH bongos cannot check")
                return found
        return os.defpath

    def resolve_program(program, env, cwd):
        """The absolute path the OS would run, without symlinks resolved, or None."""
        if os.sep not in program:
            return lookup(program, exec_path(env))
        base = text(cwd) if cwd is not None else os.getcwd()
        if base is None:
            deny("a program started in a directory bongos cannot check")
        return os.path.abspath(os.path.join(base, program))

    def check_shell_line(line, env=None, cwd=None):
        if any(char in shell_syntax for char in line):
            deny("shell command: " + line.strip()[:80])
        try:
            words = shlex.split(line)
        except ValueError:
            deny("shell command: " + line.strip()[:80])
        if words:
            # The shell looks the command up on the PATH of the environment it was given.
            check_argv(words[0], words, env, cwd)

    def check_argv(program, argv, env=None, cwd=None):
        argv = [text(arg) for arg in argv or []]
        if program is None:
            if not argv:
                return
            program = argv[0]
        else:
            program = text(program)
        if program is None:
            deny("a program bongos cannot check")
        check_env(env, program)
        found = resolve_program(program, env, cwd)
        name = os.path.basename(program)
        if found is not None and not under(found, writable):
            real = norm(found)
            if not under(real, writable):
                if real in allowed_programs and os.path.basename(found) in command_names:
                    # A multi-call binary picks its behavior from argv[0].
                    if not argv or argv[0] is None or os.path.basename(argv[0]) in command_names:
                        return
                # A shell only as `<shell> -c <line> [arg0 args...]`: any option or script before
                # -c (a script file, --rcfile, -i, --init-file) is refused.
                if (
                    real in shells
                    and len(argv) >= 3
                    and argv[0] is not None
                    and os.path.basename(argv[0]) in shell_names
                    and argv[1] == "-c"
                ):
                    if argv[2] is None:
                        deny("shell command bongos cannot check")
                    check_shell_line(argv[2], env, cwd)
                    return
        deny(name)

    def on_popen(args):
        executable, argv = args[0], args[1]
        cwd = args[2] if len(args) > 2 else None
        env = args[3] if len(args) > 3 else None
        if type(argv) in (str, bytes):
            argv = shlex.split(text(argv))
        elif type(argv) not in (list, tuple):
            deny("a program bongos cannot check")
        check_argv(executable, list(argv), env, cwd)

    def on_system(args):
        command = text(args[0])
        if command is None:
            deny("shell command bongos cannot check")
        check_shell_line(command)

    def argv_list(argv):
        if argv is None:
            return []
        if type(argv) not in (list, tuple):
            deny("a program bongos cannot check")
        return list(argv)

    def on_exec(args):
        check_argv(args[0], argv_list(args[1]), args[2] if len(args) > 2 else None)

    def on_spawn(args):
        check_argv(args[1], argv_list(args[2]), args[3] if len(args) > 3 else None)

    def on_putenv(args):
        name, value = text(args[0]), text(args[1])
        if name is None or value is None:
            deny("an environment variable bongos cannot check")
        if risky(name, True):
            deny("setting " + name)
        if name == "PATH":
            for entry in value.split(os.pathsep):
                if not entry or not os.path.isabs(entry) or under(norm(entry), writable):
                    deny("a writable PATH entry: " + entry)

    def plain(value):
        """`value` as exact bytes, converted outside the hook (so the script's own __fspath__
        may run, once) and passed on as checked; None stays None."""
        if value is None:
            return None
        kind = type(value)
        if kind is not str and kind is not bytes and not issubclass(kind, (str, bytes)):
            value = os.fspath(value)
            kind = type(value)
        if kind is str or issubclass(kind, str):
            return str.encode(str.__str__(value), fs_encoding, "surrogateescape")
        if kind is bytes or issubclass(kind, bytes):
            return bytes.__getitem__(value, slice(None))
        deny("a program bongos cannot check")

    def plain_list(values):
        if values is None:
            return None
        if type(values) not in (list, tuple):
            deny("a program bongos cannot check")
        return [plain(value) for value in values]

    def checked_fork_exec(real):
        """The fork_exec under subprocess, which raises no audit event, with the checks of
        subprocess.Popen in front: every way to it (subprocess._fork_exec, the
        _posixsubprocess module, multiprocessing) starts only what Popen could."""

        def fork_exec(*args):
            args = list(args)
            if len(args) < 6:
                deny("a program bongos cannot check")
            argv = plain_list(args[0])
            candidates = plain_list(args[1]) or []
            cwd = plain(args[4])
            env_list = plain_list(args[5])
            env = None
            if env_list is not None:
                env = {}
                for item in env_list:
                    name, _, value = item.partition(b"=")
                    env[decode(name)] = decode(value)
            base = decode(cwd) if cwd is not None else os.getcwd()
            found = None
            for candidate in candidates:
                path = os.path.join(base, decode(candidate))
                if under(norm(os.path.dirname(path)), writable):
                    deny("command from a writable PATH entry: " + os.path.basename(path))
                if os.path.isfile(path) and os.access(path, os.X_OK):
                    found = path
                    break
            if found is not None:
                check_argv(found, [decode(arg) for arg in argv or []], env, base)
            args[0] = tuple(argv) if argv is not None else None
            args[1] = tuple(candidates)
            args[4] = cwd
            args[5] = env_list
            return real(*args)

        return fork_exec

    real_fork_exec = getattr(subprocess, "_fork_exec", None)
    real_module = sys.modules.get("_posixsubprocess")
    if real_fork_exec is None and real_module is not None:
        real_fork_exec = getattr(real_module, "fork_exec", None)
    if real_fork_exec is not None:
        fork_exec = checked_fork_exec(real_fork_exec)
        stand_in = types.ModuleType("_posixsubprocess")
        stand_in.fork_exec = fork_exec
        sys.modules["_posixsubprocess"] = stand_in
        if hasattr(subprocess, "_fork_exec"):
            subprocess._fork_exec = fork_exec
        if hasattr(subprocess, "_posixsubprocess"):
            subprocess._posixsubprocess = stand_in
    del real_module

    # os.mkfifo and os.mknod raise no audit event: wrapped in os and posix.

    def checked_make(real, default_mode):
        def make(path, mode=default_mode, *rest, dir_fd=None, **options):
            path = plain(path)
            if dir_fd is not None and type(dir_fd) is not int:
                deny("write relative to a directory descriptor bongos cannot check")
            check_write(path, dir_fd)
            return real(path, mode, *rest, dir_fd=dir_fd, **options)

        return make

    native = sys.modules.get(os.name)
    for name, default_mode in (("mkfifo", 0o666), ("mknod", 0o600)):
        real = getattr(os, name, None)
        if real is not None:
            wrapped = checked_make(real, default_mode)
            setattr(os, name, wrapped)
            if native is not None and getattr(native, name, None) is real:
                setattr(native, name, wrapped)

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
        host = text(args[0])
        if not hosts or host is None or not host_allowed(host):
            return
        try:
            for info in socket.getaddrinfo(host, None):
                allowed_ips.add(info[4][0])
        except OSError:
            pass

    def check_address(address):
        if address is None:
            return
        if type(address) is tuple and len(address) >= 2:
            host, port = text(address[0]), address[1]
            where = "%s:%s" % (host, port if type(port) is int else "?")
            if hosts and host is not None and type(port) is int:
                if (host, port) in proxies or host in allowed_ips or host_allowed(host):
                    return
        else:
            where = text(address) or "an address bongos cannot check"
        deny("network " + where)

    def on_connect(args):
        check_address(args[1])

    # The hook's own state.

    def from_shim(frame):
        """Whether `frame` runs the hook (the frames of `_main` hold nothing of it)."""
        try:
            code = frame.f_code
            return norm(code.co_filename) == shim_file and code.co_name not in ("_main", "<module>")
        except Exception:
            return True

    def on_getframe(args):
        if args and from_shim(args[0]):
            deny("inspecting the bongos shim")

    def on_getattr(args):
        obj, name = args[0], args[1]
        if name == "tb_frame" and from_shim(obj.tb_frame):
            deny("inspecting the bongos shim")

    def refuse(what):
        def handler(_args):
            deny(what)

        return handler

    def on_import(args):
        if args[0] in ("ctypes", "_ctypes", "_posixsubprocess"):
            deny("import " + args[0])

    handlers = {
        "open": on_open,
        "os.remove": lambda args: check_write(args[0], args[1]),
        "os.rmdir": lambda args: check_write(args[0], args[1]),
        "os.mkdir": lambda args: check_write(args[0], args[2]),
        "os.rename": on_rename,
        "os.link": on_link,
        "os.symlink": on_symlink,
        "os.truncate": lambda args: check_write(args[0]),
        "os.chmod": lambda args: check_write(args[0], args[2]),
        "os.chown": lambda args: check_write(args[0], args[3]),
        "os.utime": lambda args: check_write(args[0], args[3]),
        "os.chflags": lambda args: check_write(args[0]),
        "os.lchflags": lambda args: check_write(args[0]),
        "os.setxattr": lambda args: check_write(args[0]),
        "os.removexattr": lambda args: check_write(args[0]),
        "subprocess.Popen": on_popen,
        "os.system": on_system,
        "os.exec": on_exec,
        "os.posix_spawn": on_exec,
        "os.spawn": on_spawn,
        "os.putenv": on_putenv,
        "socket.getaddrinfo": on_getaddrinfo,
        "socket.connect": on_connect,
        "socket.sendto": on_connect,
        "socket.sendmsg": on_connect,
        "gc.get_objects": refuse("gc.get_objects"),
        "gc.get_referrers": refuse("gc.get_referrers"),
        "gc.get_referents": refuse("gc.get_referents"),
        "ctypes.dlopen": refuse("ctypes"),
        "ctypes.dlsym": refuse("ctypes"),
        "ctypes.call_function": refuse("ctypes"),
        "ctypes.cdata": refuse("ctypes"),
        "sys._current_frames": refuse("sys._current_frames"),
        "sys.settrace": refuse("sys.settrace"),
        "sys.setprofile": refuse("sys.setprofile"),
        "sys._getframe": on_getframe,
        "object.__getattr__": on_getattr,
        "import": on_import,
    }
    # The handlers never call the script's code (see `text`). An event raised while its own
    # handler runs (the handler's own getaddrinfo or tb_frame read) is not checked again; any
    # other event is, so code that did run inside a handler gains nothing.
    def hook(event, args):
        handler = handlers.get(event)
        if handler is None:
            return
        active = getattr(local, "active", None)
        if active is None:
            active = local.active = set()
        if event in active:
            return
        active.add(event)
        try:
            handler(args)
        finally:
            active.discard(event)

    sys.addaudithook(hook)

    def end():
        try:
            os.write(denials_fd, b'{"end": true}\n')
        except OSError:
            pass

    return end


def fcntl_dup_high(fd):
    """A copy of `fd` at 200 or above, or None when the platform has no fcntl."""
    try:
        import fcntl
    except ImportError:
        return None
    return fcntl.fcntl(fd, fcntl.F_DUPFD_CLOEXEC, 200)


def _main():
    import json
    import runpy

    if len(sys.argv) < 3:
        sys.stderr.write("usage: shim.py <policy.json> <script>\n")
        sys.exit(2)
    policy_path, script = sys.argv[1], sys.argv[2]
    with open(policy_path, "r", encoding="utf-8") as policy_file:
        policy = json.load(policy_file)
    end = _install(policy)
    sys.argv = [script] + sys.argv[3:]
    try:
        runpy.run_path(script, run_name="__main__")
    except SystemExit:
        end()
        raise
    end()


if __name__ == "__main__":
    _main()
