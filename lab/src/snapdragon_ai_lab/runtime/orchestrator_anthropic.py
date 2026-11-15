"""Opt-in synchronous facade over the SDK's cancellable async request lifetime.

One owned event-loop thread per forwarded request. No shared client, credential
cache, retries or provider translation. The existing EOF watcher calls shutdown;
async task cancellation closes requests even while waiting for response headers.
"""
import asyncio
import queue
import threading


class Canceled(Exception):
    pass


class Client:
    @staticmethod
    def interrupted(error):
        return isinstance(error, (Canceled, BrokenPipeError, ConnectionResetError))

    def __init__(self, item, key, timeout):
        self.item, self.key, self.timeout = item, key, timeout
        self.messages = self
        self.events = queue.Queue(maxsize=16)
        self.stopped = threading.Event()
        self.canceled = threading.Event()
        self.lock = threading.Lock()
        self.loop = self.task = self.thread = None
        self.error = None

    def create(self, **params):
        if self.thread is not None:
            raise RuntimeError('one SDK request per owner')
        self.thread = threading.Thread(target=self._run, args=(params,),
                                       name='local-ai-anthropic-request', daemon=True)
        self.thread.start()
        kind, value = self._next()
        if kind == 'stream':
            return self
        return value

    def __iter__(self):
        return self

    def __next__(self):
        kind, value = self._next()
        if kind != 'event':
            raise RuntimeError('unexpected SDK stream state')
        return value

    def _next(self):
        while True:
            try:
                return self.events.get(timeout=0.05)
            except queue.Empty:
                if self.stopped.is_set():
                    # A producer may publish its last event just before stopping.
                    if not self.events.empty():
                        continue
                    if self.error is not None:
                        raise self.error
                    raise StopIteration

    async def _put(self, kind, value):
        while True:
            try:
                self.events.put_nowait((kind, value))
                return
            except queue.Full:
                # Never block the loop: cancellation must work under backpressure.
                await asyncio.sleep(0.01)

    async def _request(self, params):
        import anthropic
        async with anthropic.AsyncAnthropic(api_key=self.key,
                base_url=self.item['base_url'], max_retries=0, timeout=self.timeout) as client:
            if self.canceled.is_set():
                raise Canceled('client disconnected')
            result = await client.messages.create(**params)
            if not params.get('stream'):
                await self._put('message', result)
                return
            try:
                await self._put('stream', None)
                async for event in result:
                    await self._put('event', event)
            finally:
                await result.close()

    def _run(self, params):
        loop = asyncio.new_event_loop()
        try:
            asyncio.set_event_loop(loop)
            with self.lock:
                self.loop = loop
                self.task = loop.create_task(self._request(params))
                if self.canceled.is_set():
                    self.task.cancel()
            loop.run_until_complete(self.task)
        except asyncio.CancelledError:
            self.error = Canceled('client disconnected')
        except Exception as error:
            self.error = error
        finally:
            with self.lock:
                self.loop = self.task = None
            try:
                loop.run_until_complete(loop.shutdown_asyncgens())
                loop.run_until_complete(loop.shutdown_default_executor())
            except Exception as error:
                if self.error is None:
                    self.error = error
            finally:
                loop.close()
                self.stopped.set()

    def shutdown(self, _how=None):
        self.canceled.set()
        with self.lock:
            if self.loop is not None and self.task is not None:
                self.loop.call_soon_threadsafe(self.task.cancel)

    def close(self):
        self.shutdown()
        if self.thread is not None:
            self.thread.join()
        self.key = None
