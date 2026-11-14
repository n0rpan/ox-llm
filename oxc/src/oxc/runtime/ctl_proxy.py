"""The proxy to the loaded engine: request defaults per engine and `ProxyRoutes.proxy_request`,
which runs the three proxy stages in order:
  ctl_proxy_prep    request prep: checks, body, payload shaping, Flash/cache answers, backend headers
  ctl_proxy_ledger  ledger entry, admission through the Manager, the bookkeeping after the reply
  ctl_proxy_stream  forwarding: send, stream the answer, client disconnects, preemption of a
                    shared (LAN) request
"""
import time
if __package__:
    from .ctl_proxy_prep import ANSWERED, FLASH_CHAT_HTML, HOP_HEADERS, ProxyRequest, _BadFlashChat
    from . import classic_beta, ctl_proxy_ledger, ctl_proxy_prep, ctl_proxy_stream, ctl_queue, ctl_replay, oxe_runtime
else:
    from ctl_proxy_prep import ANSWERED, FLASH_CHAT_HTML, HOP_HEADERS, ProxyRequest, _BadFlashChat
    import classic_beta
    import ctl_proxy_ledger
    import ctl_proxy_prep
    import ctl_proxy_stream
    import ctl_queue
    import ctl_replay
    import oxe_runtime


def request_defaults(target, payload, path):
    """Backend request defaults for the resolved target (qwen-local is already resolved).

    The classic beta's 2048 output default belongs to its llama.cpp engine only; an
    oxe-backed Ornith gets the oxe defaults (per-profile default_max_tokens) instead.
    Returns payload itself when no backend defaults apply.
    """
    if target.get('backend_kind') == oxe_runtime.KIND:
        return oxe_runtime.request_defaults(payload, path, target)
    if classic_beta.is_beta(target):
        return classic_beta.request_defaults(payload, path, alias=target['model_alias'])
    return payload


class ProxyRoutes:
    """Handler: forwarding to the engine."""

    def proxy_request(self, path, api_value, api_key, remote, inference, helper):
        """Shape the request for the target engine, admit it, forward it and stream the answer."""
        request = ProxyRequest(path, api_value, api_key, remote, inference, helper)
        # v0.1.4 CORE-ROBUST (queue, replay, diagnostics; each behind its own flag, default off)
        request.robust = ctl_queue.Extras()
        self.early_stream = None  # the handler persists across keep-alive requests
        try:
            started = time.monotonic()
            if ctl_proxy_prep.prepare(self, request) is ANSWERED:
                return
            request.prep_s = time.monotonic() - started  # v0.2.0 #8: body read + shaping
            ctl_proxy_ledger.open_entry(self, request)
            if ctl_replay.try_attach(self, request) is ANSWERED:
                return
            while True:
                if ctl_proxy_ledger.admit(self, request) is ANSWERED:
                    return
                started = time.monotonic()
                if ctl_proxy_prep.backend_request(self, request) is ANSWERED:
                    return
                ctl_proxy_ledger.note_backend_prep(request, time.monotonic() - started)
                outcome = None
                try:
                    outcome = ctl_proxy_stream.forward(self, request)
                finally:
                    if outcome is not ctl_queue.REQUEUE:
                        ctl_proxy_ledger.finish(self, request)
                # EG1: a background request cut before its first data byte waits again
                if outcome is not ctl_queue.REQUEUE or not ctl_proxy_ledger.release_for_requeue(self, request):
                    return
        finally:
            self.early_stream = None
