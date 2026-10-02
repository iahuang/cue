#!/usr/bin/env python3
"""Run a command behind a simulated network link, like ssh over bad wifi.

    python3 tools/netsim.py -- target/release/cue .

The command gets its own pty. Everything between it and your terminal
crosses a simulated link: a fixed round trip, limited bandwidth, a stall
every few seconds (a wifi hiccup), and optional loss. Output from the
command is read as fast as it's written, the way sshd drains a pty into
its channel, so the command never feels the link: frames queue up in the
link instead.

It models queueing only, not TCP: no congestion window, no slow start.
Loss costs a packet one retransmit timeout and holds up everything after
it, as on a TCP stream.

A summary goes to stderr on exit; --log writes a sample every 100ms.
"""

import argparse
import collections
import fcntl
import os
import pty
import random
import select
import signal
import struct
import sys
import termios
import time
import tty

MSS = 1400
FRAME_END = b"\x1b[?2026l"


class Link:
    """One direction of the link. Packets go out one at a time at the
    link's bandwidth and arrive in order, a one-way delay later."""

    def __init__(self, args, epoch):
        self.delay = args.rtt / 2000
        self.bits_per_second = args.mbit * 1e6
        self.stall_every = args.stall_every
        self.stall = args.stall / 1000
        self.loss = args.loss / 100
        self.rto = args.rto / 1000
        self.epoch = epoch
        # (arrives, data, sent, ends a frame)
        self.packets = collections.deque()
        self.free = 0.0
        self.last_arrival = 0.0
        self.queued = 0
        self.lost = 0

    def after_stall(self, t):
        """The first moment at or after t that the link isn't stalled.
        The stall closes out each period."""
        if not self.stall_every or not self.stall:
            return t
        phase = (t - self.epoch) % self.stall_every
        start = self.stall_every - self.stall
        return t + (self.stall_every - phase) if phase >= start else t

    def send(self, data, now):
        for i in range(0, len(data), MSS):
            packet = data[i : i + MSS]
            start = self.after_stall(max(now, self.free))
            if self.bits_per_second:
                self.free = start + len(packet) * 8 / self.bits_per_second
            else:
                self.free = start
            arrives = self.free + self.delay
            if self.loss and random.random() < self.loss:
                arrives += self.rto
                self.lost += 1
            arrives = max(self.after_stall(arrives), self.last_arrival)
            self.last_arrival = arrives
            self.packets.append((arrives, packet, now, FRAME_END in packet))
            self.queued += len(packet)

    def due(self, now):
        """Packets that have arrived by now, oldest first."""
        while self.packets and self.packets[0][0] <= now:
            packet = self.packets.popleft()
            self.queued -= len(packet[1])
            yield packet

    def next_arrival(self):
        return self.packets[0][0] if self.packets else None


def window_size(fd):
    return fcntl.ioctl(fd, termios.TIOCGWINSZ, b"\0" * 8)


def percentile(values, p):
    if not values:
        return 0.0
    values = sorted(values)
    return values[min(len(values) - 1, int(len(values) * p))]


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--rtt", type=float, default=55, help="round trip, ms (55)")
    parser.add_argument("--mbit", type=float, default=4, help="bandwidth each way, Mbit/s; 0 for unlimited (4)")
    parser.add_argument("--stall-every", type=float, default=3, help="seconds between stalls; 0 for none (3)")
    parser.add_argument("--stall", type=float, default=600, help="stall length, ms (600)")
    parser.add_argument("--loss", type=float, default=0, help="packet loss, percent (0)")
    parser.add_argument("--rto", type=float, default=200, help="retransmit timeout for a lost packet, ms (200)")
    parser.add_argument("--log", help="write a sample every 100ms to this file")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("no command; put it after --")
    if not os.isatty(0):
        parser.error("stdin isn't a terminal")

    pid, master = pty.fork()
    if pid == 0:
        try:
            os.execvp(command[0], command)
        finally:
            os._exit(127)

    resized = True

    def on_resize(*_):
        nonlocal resized
        resized = True

    signal.signal(signal.SIGWINCH, on_resize)

    epoch = time.monotonic()
    down = Link(args, epoch)
    up = Link(args, epoch)
    log = open(args.log, "w") if args.log else None
    if log:
        log.write("seconds\tqueued_bytes\toldest_ms\n")
    next_sample = epoch
    sent_down = sent_up = 0
    peak_queued = 0
    frame_lags = []

    saved = termios.tcgetattr(0)
    tty.setraw(0)
    child_open = True
    try:
        while child_open or down.packets:
            if resized:
                resized = False
                fcntl.ioctl(master, termios.TIOCSWINSZ, window_size(0))

            now = time.monotonic()
            arrivals = [t for t in (down.next_arrival(), up.next_arrival()) if t]
            timeout = min([0.1] + [max(0.0, t - now) for t in arrivals])
            readable = [0, master] if child_open else []
            try:
                ready, _, _ = select.select(readable, [], [], timeout)
            except InterruptedError:
                ready = []

            now = time.monotonic()
            if 0 in ready:
                data = os.read(0, 65536)
                up.send(data, now)
                sent_up += len(data)
            if master in ready:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    data = b""
                if data:
                    down.send(data, now)
                    sent_down += len(data)
                else:
                    # Exited: deliver what's left now, so the terminal
                    # gets put back without waiting out the link.
                    child_open = False
                    for packet in list(down.packets):
                        os.write(1, packet[1])
                    down.packets.clear()
                    break
            peak_queued = max(peak_queued, down.queued)

            for arrives, data, sent, ends_frame in down.due(now):
                os.write(1, data)
                if ends_frame:
                    frame_lags.append(arrives - sent)
            for _, data, _, _ in up.due(now):
                try:
                    os.write(master, data)
                except OSError:
                    pass

            if log and now >= next_sample:
                oldest = (now - down.packets[0][2]) * 1000 if down.packets else 0
                log.write(f"{now - epoch:.1f}\t{down.queued}\t{oldest:.0f}\n")
                log.flush()
                next_sample = now + 0.1
    finally:
        termios.tcsetattr(0, termios.TCSAFLUSH, saved)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass

    elapsed = time.monotonic() - epoch
    floor = args.rtt / 2
    print(
        f"netsim: {elapsed:.0f}s, {sent_down / 1024:.0f} KiB down, {sent_up / 1024:.1f} KiB up, "
        f"{len(frame_lags)} frames, {down.lost} packets lost\n"
        f"  frame delay (cue writes it -> terminal gets it): "
        f"p50 {percentile(frame_lags, 0.5) * 1000:.0f}ms, "
        f"p95 {percentile(frame_lags, 0.95) * 1000:.0f}ms, "
        f"max {max(frame_lags, default=0) * 1000:.0f}ms (link floor {floor:.0f}ms)\n"
        f"  peak backlog: {peak_queued / 1024:.0f} KiB",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
