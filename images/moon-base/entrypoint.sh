#!/usr/bin/env bash
# moon-base entrypoint.
#
# Backgrounds dockerd so the user's project can `docker run` /
# `docker compose up` inside the workspace container, then hands
# off to whatever was actually requested (CMD, or the argv of a
# `docker exec`).
#
# Requires `privileged: true` on the compose service (see
# specs/containers.md). Without it dockerd will fail and log to
# /var/log/dockerd.log; the rest of the container still works for
# non-Docker workloads, so we don't make this fatal.

set -eo pipefail

if command -v dockerd >/dev/null 2>&1; then
	# `dev` has passwordless sudo from the image build; dockerd
	# itself runs as root and the socket ends up root:docker 660,
	# which the dev user can use via the docker group. The
	# redirection has to happen inside the sudo'd shell so root
	# (not dev) opens the log file under /var/log/.
	sudo sh -c 'dockerd --host=unix:///var/run/docker.sock >/var/log/dockerd.log 2>&1 &'

	# Wait up to ~15 s for the socket to be ready. If the daemon
	# can't come up (e.g. container isn't privileged) we still
	# proceed — the user will see the error the first time they
	# actually try to use docker.
	for _ in $(seq 1 30); do
		if docker info >/dev/null 2>&1; then
			break
		fi
		sleep 0.5
	done
fi

exec "$@"
