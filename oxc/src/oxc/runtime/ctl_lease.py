"""The inference lease and admission. One file lock is shared with image jobs; inference requests
take turns in FIFO order; an NPU image borrows the lease while the chat model stays resident.
"""
from contextlib import contextmanager
import errno
import fcntl
import os
from pathlib import Path
import time
if __package__:
    from .ctl_env import Unavailable
    from .ctl_seams import ctl
    from . import ctl_queue, image_npu
else:
    from ctl_env import Unavailable
    from ctl_seams import ctl
    import ctl_queue
    import image_npu


def open_lease_rw():
    """Audit N7: the lease read/write for a streamed (BigMoe Flash) backend, created 0600 when a
    fresh state directory has none yet (it was opened without O_CREAT and failed there)."""
    ctl.LEASE.parent.mkdir(parents=True, exist_ok=True)
    try:
        return os.fdopen(os.open(ctl.LEASE, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600), 'r+')
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise Unavailable(f'The inference lease {ctl.LEASE} is a symbolic link; refusing to use it. '
                              'Remove the link and retry.') from None
        raise


def open_lease_append():
    """N6: the lease for appending, created 0600, never through a symlink (a planted link is refused)."""
    try:
        return os.fdopen(os.open(ctl.LEASE, os.O_WRONLY | os.O_APPEND | os.O_CREAT | os.O_NOFOLLOW, 0o600), 'a')
    except OSError as error:
        if error.errno == errno.ELOOP:
            raise Unavailable(f'The inference lease {ctl.LEASE} is a symbolic link; refusing to use it. '
                              'Remove the link and retry.') from None
        raise


class LeaseMixin:
    """Manager: the lease, the request queue and the image slot."""

    @contextmanager
    def reserve_image(self, profile, peer_pid):
        """Delegate the existing lease while keeping the chat model resident.

        The image worker inherits the lease descriptor and control socket. Chat
        requests use the existing FIFO until that worker has finished. Images
        do not jump ahead of queued chat requests or unload a resident model.
        """
        if os.environ.get('OXC_IMAGE_NPU') != '1' or profile != 'klein-q8-512-npu':
            raise Unavailable('NPU image slots are not enabled.')
        owned_lease = None
        activated = False
        reserved = False
        with self.queue_cv:
            if self.queue_reserved or self.queue_waiters or self.inferences:
                raise Unavailable('Chat is busy. Try the image again when it finishes.')
            self.queue_reserved = True
            reserved = True
        try:
            with self.mu:
                if self.active or self.image_active:
                    raise Unavailable('A model operation is already active.')
                if self.stop_pending is not None:
                    # Audit N6: an engine whose exit is unproven still holds its memory; the lease
                    # must not be handed to an image worker until the stop is proven.
                    raise Unavailable('The previous model has not exited yet. Try again in a moment.')
                if self.process is not None and self.process.poll() is not None:
                    raise Unavailable('The resident engine needs recovery before an image.')
                lease = self.lease
                if lease is None:
                    if self.process is not None:
                        raise Unavailable('Resident model has no canonical lease.')
                    ctl.guard_production_path(ctl.LEASE)
                    ctl.LEASE.parent.mkdir(parents=True, exist_ok=True)
                    ctl.guard_production_path(ctl.LEASE)
                    fd = os.open(ctl.LEASE, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
                    owned_lease = lease = os.fdopen(fd, 'a+')
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                resident = ctl.resident_bytes(self.process.pid) if self.process is not None else None
                if self.process is not None and resident is None:
                    raise Unavailable('Resident-model memory telemetry is unavailable.')
                choice = image_npu.choose(enabled=True, ready=True,
                    latched=(Path.home() / '.local/state/oxc/NPU-STOP').exists(),
                    mem_available=ctl.available_memory_bytes(), mem_total=ctl.total_memory_bytes(),
                    resident_model_bytes=resident, swap_free=ctl.swap_bytes()[1])
                metadata = {'backend': choice.backend, 'reason': choice.reason,
                            'required_bytes': choice.required_bytes, 'chat_retained': self.process is not None}
                self.image_active = True
                self.active += 1
                self.inferences += 1
                activated = True
            with self.queue_cv:
                self.queue_reserved = False
                reserved = False
                self.queue_cv.notify_all()
            yield lease.fileno(), metadata
        finally:
            if owned_lease is not None:
                owned_lease.close()
            if activated:
                with self.mu:
                    self.image_active = False
                self.end(inference=True, touch=True)
            with self.queue_cv:
                if reserved:
                    self.queue_reserved = False
                self.queue_cv.notify_all()

    @contextmanager
    def held_lease(self):
        """The canonical inference lease for a cache mutation, held for the whole block: the
        loaded model's (this controller already owns it) or taken here, non-blocking. Call with
        mu held. Unavailable when another image, chat or setup job holds it."""
        if self.lease is not None:
            yield  # a loaded model already owns the lease
            return
        ctl.guard_production_path(ctl.LEASE)
        ctl.LEASE.parent.mkdir(parents=True, exist_ok=True)
        with ctl.open_lease_append() as lease:
            try:
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise Unavailable('Another image, chat or setup job is running. Try again when it finishes.') from None
            yield

    def queue_length(self):
        with self.queue_cv:
            return len(self.queue_waiters)

    def begin_queued(self, *, inference=False, allow_start=False, model_alias=None, wait_seconds=None, origin=None,
                     remote=False, preempt=None, waiter=None):
        """begin(), but an inference request that finds another one running waits for its
        turn (first come, first served) for up to QUEUE_WAIT_SECONDS before failing.

        O-102: `remote` (a shared request) is admitted only to the loaded model (ModelBusy, checked
        in begin under mu). Its `preempt` (lan_share.Preempt) is registered in the same step that
        admits it, and a local inference request that enters the queue while it runs cancels it
        (M4: local use first); a local request already waiting keeps a shared one from being admitted.

        v0.1.4 CORE-ROBUST: a `waiter` (ctl_queue.Waiter, OXC_QUEUE=1, local requests only)
        waits in ctl_queue's round-robin queue instead (early SSE headers, no stream timeout, cap)."""
        flags = {'remote': True} if remote else {}
        if not inference:
            return self.begin(inference=False, allow_start=allow_start, model_alias=model_alias, origin=origin, **flags)
        entered = time.monotonic()
        if waiter is not None and not remote:
            ctl_queue.wait_turn(self, waiter, preempt=preempt)
            return self._admit_reserved(entered, allow_start, model_alias, origin, flags, preempt)
        wait = ctl.QUEUE_WAIT_SECONDS if wait_seconds is None else wait_seconds
        deadline = time.monotonic() + wait
        ticket = object()
        with self.queue_cv:
            self.queue_waiters.append(ticket)
            running = getattr(self, 'remote_preempt', None)
            if not remote and running is not None:
                running.cancel()
            if not remote:
                self.queue_cv.notify_all()  # a shared request waiting for its turn gives way (below)
            try:
                while not (self.queue_waiters[0] is ticket and not self.queue_reserved and self.inferences == 0
                           and not (remote and len(self.queue_waiters) > 1)):
                    if remote and len(self.queue_waiters) > 1:
                        # a shared request never waits ahead of (or among) local ones: it may only
                        # wait out the end of the request before it (SLOT_WAIT)
                        raise Unavailable('The host is busy. Try again in a moment.')
                    left = deadline - time.monotonic()
                    if left <= 0:
                        raise Unavailable(f'Another chat request is still running after {wait:.0f} s. '
                                          'Try again when it finishes.')
                    self.queue_cv.wait(min(left, 0.5))
                self.queue_reserved = True
                if preempt is not None:
                    self.remote_preempt = preempt
            finally:
                self.queue_waiters.remove(ticket)
                self.queue_cv.notify_all()
        return self._admit_reserved(entered, allow_start, model_alias, origin, flags, preempt)

    def _admit_reserved(self, entered, allow_start, model_alias, origin, flags, preempt):
        """The turn is reserved (queue_reserved): begin(), then release the reservation."""
        admitted = time.monotonic()
        try:
            try:
                process = self.begin(inference=True, allow_start=allow_start, model_alias=model_alias, origin=origin, **flags)
            except BaseException:
                with self.queue_cv:
                    if preempt is not None and self.remote_preempt is preempt:
                        self.remote_preempt = None
                raise
            # For the request ledger: (seconds queued, seconds in begin incl. a model load).
            # Only the admitted request reads it; the next admission needs this one to end.
            self.admission_times = (admitted - entered, time.monotonic() - admitted)
            self.answering_since = time.monotonic()
            return process
        finally:
            with self.queue_cv:
                self.queue_reserved = False
                self.queue_cv.notify_all()

    def begin_finalizing(self):
        """The caller's request has sent its last byte and its engine stream is over: only bookkeeping is left."""
        with self.mu:
            self.finalizing += 1

    def wait_finalizing(self, timeout):
        """(mu held) Wait up to `timeout` s while EVERY running request is only finalizing. True when none is
        left; False at once when one is still generating (never unload under it), or when the wait expires."""
        deadline = time.monotonic() + timeout
        while self.active:
            left = deadline - time.monotonic()
            if self.active != getattr(self, 'finalizing', 0) or left <= 0:
                return False
            self.mu_cv.wait(left)
        return True

    def end(self, *, inference=False, touch=False, finalizing=False):
        with self.mu:
            if finalizing:
                self.finalizing -= 1
            self.active -= 1
            if inference:
                self.inferences -= 1
                self.answering_since = None
                self.remote_preempt = None  # one inference at a time: a registered one was this one
                ctl_queue.released(self)  # EG1: likewise the registered background run
            if inference or touch:
                self.last_used = time.monotonic()
            cv = getattr(self, 'mu_cv', None)
            if cv is not None:
                cv.notify_all()
        if inference:
            with self.queue_cv:
                self.queue_cv.notify_all()
