#!/bin/sh
# Login shell of the remote user. userEnvProbe runs through it, so the variables it
# exports reach a command only through the probe.
export DECUNE_PROBED=from-login-shell
export DECUNE_SHARED=from-login-shell
exec /bin/sh "$@"
