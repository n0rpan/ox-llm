# Retired controller translator entry points

The Anthropic request and stream implementations now live in
[`oxa/oxc-gateway`](../oxa/oxc-gateway/README.md), in the oxa workspace.
The existing optional library features re-export those modules; no translator is
implemented here. Native gateway tests cover provider behavior. The old Python
recorders and byte-exact fixtures have been removed.
