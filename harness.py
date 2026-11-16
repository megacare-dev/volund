"""Starts Völund, the Rust harness that submission.yaml's `install` builds into /opt/harness/bin.

The harness itself is Rust (crates/volund): a WebSocket server on $PORT that speaks the PLE serve
protocol and drives the model. This launcher only replaces the Python process with that binary, so
the evaluator talks to the binary directly. VOLUND_BIN points it at another build for local runs.
"""

import os
import sys

BINARY = os.environ.get("VOLUND_BIN", "/opt/harness/bin/volund")


def main() -> None:
    if not os.access(BINARY, os.X_OK):
        sys.exit(f"volund: {BINARY} is missing or not executable; submission.yaml's install builds it")
    os.execv(BINARY, [BINARY, *sys.argv[1:]])


if __name__ == "__main__":
    main()
