#!/usr/bin/env python3
"""Read a paused Hermit/QEMU Linux guest through drgn without ptrace attach.

The helper restores the phase-5 QEMU snapshot with its CPUs paused.  Hermit's
exact tracer thread group is then stopped while QEMU is in ptrace-stop, and
guest physical reads are served from QEMU's RAM mmap through
``/proc/<qemu-pid>/mem``.  Only :meth:`advance` resumes the tracer and guest.
"""

from contextlib import contextmanager
from dataclasses import dataclass
import json
import lzma
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time
from typing import Iterator, Optional, Tuple


XZ_MAGIC = b"\xfd7zXZ\x00"
ELF_MAGIC = b"\x7fELF"
VMCOREINFO_MARKER = b"OSRELEASE="


@dataclass(frozen=True)
class GuestConfig:
    root: Path
    hermit: Path
    qemu: Path
    kernel: Path
    initrd: Path
    vmlinux: Path
    snapshot_disk: Path
    snapshot_name: str
    artifact_dir: Path
    qemu_bios: Optional[Path] = None
    qemu_library_path: Optional[Path] = None
    timeout: float = 240.0
    ram_bytes: int = 512 << 20


@dataclass(frozen=True)
class ObservationMetrics:
    physical_reads: int
    physical_bytes: int
    qemu_state: str
    tracer_state: str
    serial_bytes_delta: int


class QmpClient:
    """Small synchronous QMP client tolerant of interleaved events."""

    def __init__(self, connection: socket.socket) -> None:
        self.connection = connection
        self.stream = connection.makefile("rwb", buffering=0)
        greeting = self._read_message()
        if "QMP" not in greeting:
            raise RuntimeError("QMP greeting was not received")
        self.execute("qmp_capabilities")

    @classmethod
    def connect(
        cls, path: Path, process: subprocess.Popen, timeout: float
    ) -> "QmpClient":
        deadline = time.monotonic() + timeout
        last_error = None
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(
                    "Hermit exited before QMP connected (status {})".format(
                        process.returncode
                    )
                )
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(min(5.0, timeout))
            try:
                connection.connect(str(path))
                return cls(connection)
            except (FileNotFoundError, ConnectionRefusedError, socket.timeout) as error:
                last_error = error
                connection.close()
                time.sleep(0.05)
        raise TimeoutError("QMP socket did not become ready: {} ({})".format(path, last_error))

    def _read_message(self):
        while True:
            line = self.stream.readline()
            if not line:
                raise RuntimeError("QMP disconnected")
            try:
                return json.loads(line.decode("utf-8"))
            except json.JSONDecodeError:
                continue

    def execute(self, command: str, arguments=None):
        request = {"execute": command}
        if arguments:
            request["arguments"] = arguments
        self.stream.write(json.dumps(request, separators=(",", ":")).encode() + b"\n")
        while True:
            response = self._read_message()
            if "event" in response:
                continue
            if "error" in response:
                raise RuntimeError("QMP {} failed: {}".format(command, response["error"]))
            if "return" in response:
                return response["return"]

    def status(self) -> str:
        result = self.execute("query-status")
        return str(result.get("status", ""))

    def close(self) -> None:
        try:
            self.stream.close()
        finally:
            self.connection.close()


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".vmlinux.", dir=str(path.parent))
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(data)
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def ensure_vmlinux(kernel: Path, vmlinux: Path) -> Path:
    """Extract the ELF-with-BTF payload from the fixed XZ bzImage if needed."""
    if vmlinux.is_file():
        with vmlinux.open("rb") as source:
            if source.read(4) == ELF_MAGIC:
                return vmlinux
        raise RuntimeError("cached vmlinux is not an ELF file: {}".format(vmlinux))

    compressed = kernel.read_bytes()
    offset = compressed.find(XZ_MAGIC)
    if offset < 0:
        raise RuntimeError(
            "kernel has no XZ-compressed vmlinux; set DEMO07_VMLINUX to matching debug info"
        )
    try:
        extracted = lzma.decompress(compressed[offset:])
    except lzma.LZMAError as error:
        raise RuntimeError("could not extract vmlinux from {}: {}".format(kernel, error))
    if not extracted.startswith(ELF_MAGIC):
        raise RuntimeError("extracted kernel payload is not ELF")
    _atomic_write(vmlinux, extracted)
    return vmlinux


def _elf_build_id(path: Path) -> str:
    try:
        result = subprocess.run(
            ["readelf", "-n", str(path)],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        raise RuntimeError("readelf is required to verify the kernel BuildID")
    except subprocess.CalledProcessError as error:
        raise RuntimeError("readelf failed for {}: {}".format(path, error.stderr.strip()))
    match = re.search(r"Build ID:\s*([0-9a-fA-F]+)", result.stdout)
    if match is None:
        raise RuntimeError("vmlinux has no GNU BuildID: {}".format(path))
    return match.group(1).lower()


def _proc_status_value(pid: int, key: str) -> str:
    with open("/proc/{}/status".format(pid)) as status:
        for line in status:
            if line.startswith(key + ":"):
                return line.split()[1]
    raise RuntimeError("no {} in /proc/{}/status".format(key, pid))


def _proc_state(pid: int) -> str:
    return _proc_status_value(pid, "State")


def _find_qemu(process_group: int) -> Optional[int]:
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open("/proc/{}/stat".format(pid)) as stat_file:
                fields = stat_file.read().rsplit(") ", 1)[1].split()
            if int(fields[2]) != process_group:
                continue
            with open("/proc/{}/comm".format(pid)) as comm_file:
                comm = comm_file.read().strip()
            if comm.startswith("qemu-system") or comm == "qemu-kvm":
                return pid
        except (OSError, IndexError, ValueError):
            continue
    return None


def _wait_for_qemu(process: subprocess.Popen, process_group: int, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                "Hermit exited before QEMU appeared (status {})".format(process.returncode)
            )
        qemu_pid = _find_qemu(process_group)
        if qemu_pid is not None:
            return qemu_pid
        time.sleep(0.05)
    raise TimeoutError("QEMU child not found in Hermit's process group")


def _open_serial_pipe(
    base: Path, process: subprocess.Popen, timeout: float
) -> Tuple[int, int]:
    """Open a QEMU `-serial pipe:` FIFO pair (``<base>.in``/``<base>.out``).

    A connected unix-socket serial chardev keeps a host-timing poll-ready fd in
    QEMU's main loop and starves the -icount vCPU under `hermit --no-rcb-time`
    (the demo-5 boot bug). A pipe's input FIFO is poll-ready only while a command
    is queued, so QEMU's main loop blocks in poll() between commands. QEMU opens
    both ends O_RDWR at chardev init, so these opens do not block once QEMU is up
    (guaranteed by the preceding QMP connect). The read end is non-blocking so
    the advance loop can poll for guest liveness, matching the old socket's
    per-recv timeout.
    """
    read_fd = os.open(str(base) + ".out", os.O_RDONLY | os.O_NONBLOCK)
    deadline = time.monotonic() + timeout
    while True:
        try:
            write_fd = os.open(str(base) + ".in", os.O_WRONLY | os.O_NONBLOCK)
            return read_fd, write_fd
        except OSError as error:
            if process.poll() is not None:
                os.close(read_fd)
                raise RuntimeError(
                    "Hermit exited before serial pipe opened (status {})".format(
                        process.returncode
                    )
                )
            if time.monotonic() >= deadline:
                os.close(read_fd)
                raise TimeoutError(
                    "serial input pipe did not open: {}.in ({})".format(base, error)
                )
            time.sleep(0.05)


def _freeze_exact_tracer(qemu_pid: int, timeout: float = 20.0) -> Tuple[int, int]:
    tracer_tid = int(_proc_status_value(qemu_pid, "TracerPid"))
    if tracer_tid == 0:
        raise RuntimeError("QEMU is not ptrace-traced by Hermit")
    tracer_tgid = int(_proc_status_value(tracer_tid, "Tgid"))
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if _proc_state(qemu_pid) != "t":
            time.sleep(0.002)
            continue
        os.kill(tracer_tgid, signal.SIGSTOP)
        for _ in range(1000):
            if _proc_state(tracer_tgid) == "T":
                break
            time.sleep(0.001)
        if _proc_state(qemu_pid) == "t" and _proc_state(tracer_tgid) == "T":
            return tracer_tid, tracer_tgid
        os.kill(tracer_tgid, signal.SIGCONT)
    raise TimeoutError("could not freeze Hermit's exact tracer at a QEMU trace-stop")


def _ram_region(qemu_pid: int, wanted: int) -> Tuple[int, int]:
    candidates = []
    with open("/proc/{}/maps".format(qemu_pid)) as maps:
        for line in maps:
            fields = line.split()
            if "r" not in fields[1] or "w" not in fields[1]:
                continue
            first, last = (int(value, 16) for value in fields[0].split("-"))
            size = last - first
            if wanted // 2 <= size <= wanted * 2:
                candidates.append((abs(size - wanted), first, last))
    if not candidates:
        raise RuntimeError("could not identify the {} MiB QEMU RAM mmap".format(wanted >> 20))
    _, first, last = min(candidates)
    return first, last


def _scan_vmcoreinfo(
    descriptor: int, first: int, last: int, chunk: int = 8 << 20
) -> Tuple[int, bytes]:
    offset = first
    tail = b""
    while offset < last:
        count = min(chunk, last - offset)
        try:
            data = os.pread(descriptor, count, offset)
        except OSError:
            offset += 4096
            tail = b""
            continue
        if not data:
            offset += 4096
            tail = b""
            continue
        combined = tail + data
        search_from = 0
        while True:
            index = combined.find(VMCOREINFO_MARKER, search_from)
            if index < 0:
                break
            address = offset - len(tail) + index
            candidate = os.pread(descriptor, 65536, address).split(b"\0", 1)[0]
            if (
                re.match(rb"OSRELEASE=[0-9]", candidate)
                and b"\nPAGESIZE=" in candidate
                and b"\nSYMBOL(_stext)=" in candidate
            ):
                return address, candidate
            search_from = index + len(VMCOREINFO_MARKER)
        tail = data[-len(VMCOREINFO_MARKER) :]
        offset += len(data)
    raise RuntimeError("VMCOREINFO was not found in guest RAM")


class HermitGuestProgram:
    def __init__(self, config: GuestConfig) -> None:
        self.config = config
        self._process = None  # type: Optional[subprocess.Popen]
        self._process_group = None  # type: Optional[int]
        self._qemu_pid = None  # type: Optional[int]
        self._tracer_tgid = None  # type: Optional[int]
        self._memory = None  # type: Optional[int]
        self._qmp = None  # type: Optional[QmpClient]
        self._serial_read_fd = None  # type: Optional[int]
        self._serial_write_fd = None  # type: Optional[int]
        self._ram_first = 0
        self._ram_size = 0
        self._vmcoreinfo = b""
        self._vmlinux = None  # type: Optional[Path]
        self._frozen = False
        self._serial_bytes = 0
        self._reads = 0
        self._bytes = 0
        self.metrics = []  # type: list[ObservationMetrics]
        self.run_dir = None  # type: Optional[Path]
        self.serial_log = None  # type: Optional[Path]

    def start(self) -> "HermitGuestProgram":
        for path in (
            self.config.hermit,
            self.config.qemu,
            self.config.kernel,
            self.config.initrd,
            self.config.snapshot_disk,
        ):
            if not path.is_file():
                raise FileNotFoundError(str(path))
        self._vmlinux = ensure_vmlinux(self.config.kernel, self.config.vmlinux)
        self.config.artifact_dir.mkdir(parents=True, exist_ok=True)
        self.run_dir = Path(tempfile.mkdtemp(prefix="run.", dir=str(self.config.artifact_dir)))
        self.serial_log = self.run_dir / "serial.log"
        hermit_log = self.run_dir / "hermit.log"
        qmp_socket = self.run_dir / "qmp.sock"
        # Bidirectional serial over a `-serial pipe:` FIFO pair, not a unix
        # socket: a socket chardev's poll fd starves the -icount vCPU under
        # `hermit --no-rcb-time` (the demo-5 boot bug). QEMU opens (does not
        # create) the FIFOs, so make them before launch.
        serial_pipe = self.run_dir / "serial"
        for suffix in (".in", ".out"):
            os.mkfifo(str(serial_pipe) + suffix)
        working_snapshot = self.run_dir / "snapshot.qcow2"
        shutil.copyfile(str(self.config.snapshot_disk), str(working_snapshot))

        qemu_command = [
            str(self.config.qemu),
            "-machine", "q35",
            "-accel", "tcg",
            "-cpu", "max",
            "-smp", "1",
            "-m", "512M",
            "-display", "none",
            "-monitor", "none",
            "-serial", "pipe:{}".format(serial_pipe),
            "-qmp", "unix:{},server=on,wait=off".format(qmp_socket),
            "-drive", "if=none,id=hermit-snapshot-store,file={},format=qcow2".format(
                working_snapshot
            ),
            "-loadvm", self.config.snapshot_name,
            "-S",
            "-icount", "shift=0,sleep=off",
            "-rtc", "base=2022-01-01T00:00:00,clock=vm",
            "-kernel", str(self.config.kernel),
            "-initrd", str(self.config.initrd),
            "-append", "console=ttyS0 reboot=t",
        ]
        if self.config.qemu_bios is not None:
            qemu_command[1:1] = ["-L", str(self.config.qemu_bios)]
        command = [
            str(self.config.hermit),
            "run",
            "--strict",
            "--no-rcb-time",
            "--target-timeslice", "100000",
            "--max-timeslice", "disabled",
            "--",
        ] + qemu_command
        environment = os.environ.copy()
        environment["LC_ALL"] = "C"
        environment["TZ"] = "UTC"
        if self.config.qemu_library_path is not None:
            old_path = environment.get("LD_LIBRARY_PATH", "")
            environment["LD_LIBRARY_PATH"] = str(self.config.qemu_library_path) + (
                ":" + old_path if old_path else ""
            )

        with hermit_log.open("wb") as output:
            self._process = subprocess.Popen(
                command,
                cwd=str(self.config.root),
                env=environment,
                start_new_session=True,
                stdout=output,
                stderr=subprocess.STDOUT,
            )
        self._process_group = os.getpgid(self._process.pid)
        self._qmp = QmpClient.connect(qmp_socket, self._process, self.config.timeout)
        self._serial_read_fd, self._serial_write_fd = _open_serial_pipe(
            serial_pipe, self._process, self.config.timeout
        )
        initial_status = self._qmp.status()
        if initial_status not in ("paused", "prelaunch"):
            self._qmp.execute("stop")
            if self._qmp.status() != "paused":
                raise RuntimeError("QEMU did not start with guest CPUs paused")

        self._qemu_pid = _wait_for_qemu(
            self._process, self._process_group, self.config.timeout
        )
        _, self._tracer_tgid = _freeze_exact_tracer(self._qemu_pid)
        self._frozen = True
        first, last = _ram_region(self._qemu_pid, self.config.ram_bytes)
        self._ram_first = first
        self._ram_size = last - first
        self._memory = os.open("/proc/{}/mem".format(self._qemu_pid), os.O_RDONLY)
        _, self._vmcoreinfo = _scan_vmcoreinfo(self._memory, first, last)
        build_match = re.search(rb"(?m)^BUILD-ID=([0-9a-fA-F]+)$", self._vmcoreinfo)
        if build_match is None:
            raise RuntimeError("guest VMCOREINFO has no BuildID")
        guest_build_id = build_match.group(1).decode().lower()
        debug_build_id = _elf_build_id(self._vmlinux)
        if guest_build_id != debug_build_id:
            raise RuntimeError(
                "kernel/vmlinux BuildID mismatch: guest {} debug {}".format(
                    guest_build_id, debug_build_id
                )
            )
        return self

    def _program(self):
        if not self._frozen or self._memory is None or self._vmlinux is None:
            raise RuntimeError("guest must be frozen before a drgn observation")
        self._reads = 0
        self._bytes = 0

        import drgn

        def read_physical(address, count, offset, physical):
            if not physical:
                raise ValueError("guest memory callback received a virtual read")
            data = os.pread(self._memory, count, self._ram_first + offset)
            if len(data) != count:
                raise OSError(
                    "short guest RAM read at {:#x}: {}/{}".format(
                        address, len(data), count
                    )
                )
            self._reads += 1
            self._bytes += count
            return data

        program = drgn.Program(drgn.Platform(drgn.Architecture.X86_64))
        program.add_memory_segment(0, self._ram_size, read_physical, physical=True)
        program.set_linux_kernel_custom(self._vmcoreinfo, True)
        program.load_debug_info([str(self._vmlinux)], main=True)
        return program

    @contextmanager
    def observation(self):
        if self._qemu_pid is None or self._tracer_tgid is None:
            raise RuntimeError("guest was not started")
        serial_before = self._serial_bytes
        program = self._program()
        yield program
        qemu_state = _proc_state(self._qemu_pid)
        tracer_state = _proc_state(self._tracer_tgid)
        serial_delta = self._serial_bytes - serial_before
        current = ObservationMetrics(
            physical_reads=self._reads,
            physical_bytes=self._bytes,
            qemu_state=qemu_state,
            tracer_state=tracer_state,
            serial_bytes_delta=serial_delta,
        )
        self.metrics.append(current)
        if qemu_state != "t" or tracer_state != "T" or serial_delta != 0:
            raise RuntimeError(
                "guest advanced during read: qemu={} tracer={} serial_delta={}".format(
                    qemu_state, tracer_state, serial_delta
                )
            )

    def _wait_for_serial(self, marker: bytes) -> None:
        if (
            self._serial_read_fd is None
            or self.serial_log is None
            or self._process is None
        ):
            raise RuntimeError("serial transport is unavailable")
        deadline = time.monotonic() + self.config.timeout
        transcript = bytearray()
        with self.serial_log.open("ab") as output:
            while time.monotonic() < deadline:
                if self._process.poll() is not None:
                    raise RuntimeError(
                        "Hermit exited during deterministic advance (status {})".format(
                            self._process.returncode
                        )
                    )
                try:
                    chunk = os.read(self._serial_read_fd, 65536)
                except BlockingIOError:
                    # No guest output yet; QEMU still holds the pipe's write end
                    # open (O_RDWR), so this is a wait, not EOF.
                    time.sleep(0.02)
                    continue
                if not chunk:
                    raise RuntimeError("guest serial disconnected during advance")
                transcript.extend(chunk)
                self._serial_bytes += len(chunk)
                output.write(chunk)
                output.flush()
                if marker in transcript:
                    return
        raise TimeoutError("guest advance marker was not seen")

    def advance(self, command: str, marker: bytes) -> None:
        """Run one fixed guest command interval, then freeze at its marker."""
        if not self._frozen or self._serial_write_fd is None or self._qmp is None:
            raise RuntimeError("guest is not ready for deterministic advance")
        if self._tracer_tgid is None or self._qemu_pid is None:
            raise RuntimeError("traced processes are unavailable")
        if b"\n" in marker or "\n" in command or "\r" in command:
            raise ValueError("advance command and marker must each be one line")

        # Queue the deterministic input while the tracee and tracer are frozen.
        # Once the tracer resumes, QEMU can accept it but the vCPU remains paused
        # until the explicit QMP cont below.
        os.write(self._serial_write_fd, command.encode("utf-8") + b"\n")
        os.kill(self._tracer_tgid, signal.SIGCONT)
        self._frozen = False
        self._qmp.execute("cont")
        self._wait_for_serial(marker)
        self._qmp.execute("stop")
        if self._qmp.status() != "paused":
            raise RuntimeError("QEMU did not pause after deterministic advance")
        _, self._tracer_tgid = _freeze_exact_tracer(self._qemu_pid)
        self._frozen = True

    def close(self) -> None:
        if self._memory is not None:
            os.close(self._memory)
            self._memory = None
        if self._qmp is not None:
            try:
                self._qmp.close()
            except OSError:
                pass
            self._qmp = None
        for attribute in ("_serial_read_fd", "_serial_write_fd"):
            descriptor = getattr(self, attribute)
            if descriptor is not None:
                try:
                    os.close(descriptor)
                except OSError:
                    pass
                setattr(self, attribute, None)
        if self._process_group is not None:
            try:
                os.killpg(self._process_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if self._process is not None:
            try:
                self._process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass


@contextmanager
def program_from_hermit(config: GuestConfig) -> Iterator[HermitGuestProgram]:
    """Yield a restored snapshot guest, initially frozen for observation."""
    guest = HermitGuestProgram(config)
    try:
        yield guest.start()
    finally:
        guest.close()
