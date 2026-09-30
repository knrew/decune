#!/bin/sh
# Login shell of the remote user. It runs the userEnvProbe command, so the probe
# stdout carries the env listing, then writes secret values to stderr and fails.
/bin/sh "$@"
printf '%s\n' 'decune-probe-startup-failed' >&2
printf '%s\n' "$DECUNE_PROBE_SECRET" >&2
printf '%s\n' "${DECUNE_PROBE_SECRET#Bearer }" >&2
printf '%s\n' "$DECUNE_PROBE_UNREFERENCED_SECRET" >&2
exit 23
