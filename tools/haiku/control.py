#!/usr/bin/env python3
"""Drive FilmCraft's control server (`--control PORT`) on the same machine.

Feed JSON requests on stdin, one per line ({"method": ..., "params": ...}); each reply is printed
on a line of its own. The credential is FILMCRAFT_CONTROL_TOKEN from the environment (the one the
app was started with) and is added to every request; it never goes on the command line.
"""
import argparse
import json
import os
import socket
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--port', type=int, default=9876)
    parser.add_argument('--timeout', type=float, default=120)
    args = parser.parse_args()
    token = os.environ.get('FILMCRAFT_CONTROL_TOKEN', '')
    if len(token) != 64:
        sys.exit('control.py: FILMCRAFT_CONTROL_TOKEN must hold the 64 hex characters the app was started with')
    with socket.create_connection(('127.0.0.1', args.port), args.timeout) as conn:
        io = conn.makefile('rwb')
        for n, line in enumerate(sys.stdin, 1):
            if not line.strip():
                continue
            request = json.loads(line)
            request.setdefault('id', n)
            request['token'] = token
            io.write((json.dumps(request) + '\n').encode())
            io.flush()
            reply = io.readline(64 * 1024 * 1024)
            if not reply:
                sys.exit('control.py: FilmCraft closed the control connection')
            print(reply.decode().rstrip(), flush=True)


if __name__ == '__main__':
    main()
