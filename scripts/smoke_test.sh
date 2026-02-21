#!/bin/bash

# Basic connectivity test
ping -c 1 localhost || exit 1

# Service status mock check
systemctl is-active docker >/dev/null 2>&1 || echo "[WARN] Docker not active"

# Port check mock
nc -zv localhost 22 || echo "[WARN] SSH not responding"

echo "SMOKE TEST PASSED"
exit 0
