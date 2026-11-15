"""Proxy stage 2, forwarding and streaming: connect (and attach a shared request's preempt), send,
the client-disconnect watcher, the response classification (gzip UI assets, cache, the
/v1/completions shim), the chunk loop, and the client, backend and preempt failure replies.
The bookkeeping that always follows is stage 3 (ctl_proxy_ledger.finish).
"""
import gzip
import http.client
import json
if __package__:
    from .ctl_proxy_prep import HOP_HEADERS
    from .ctl_seams import ctl
    from .harness_registry import timings_from_tail
    from . import ctl_diag, ctl_queue, ctl_replay, lan_share, openai_api
else:
    from ctl_proxy_prep import HOP_HEADERS
    from ctl_seams import ctl
    from harness_registry import timings_from_tail
    import ctl_diag
    import ctl_queue
    import ctl_replay
    import lan_share
    import openai_api


def add_reply_headers(self, headers):
    """The request's conformance headers, once per name: an EG1 re-issue of the same request
    (forward() runs again) replaces an earlier value instead of repeating the header."""
    names = {name.lower() for name, _ in headers}
    current = [h for h in (getattr(self, 'reply_extra_headers', None) or ()) if h[0].lower() not in names]
    self.reply_extra_headers = current + list(headers)


def backend_failed(self, st):
    native_died = st.request_process is not None and st.request_process.poll() is not None
    if native_died:
        self.manager.record_engine_death(st.request_process, st.request_engine_config)
    st.request_error = 'backend_died'
    if not st.started_response:
        self.json_reply(502, {'error': {'message': 'Local backend connection closed.', 'type': 'backend_died'}})
    else:
        self.close_connection = True


def write_client(self, st, data):
    try:
        self.wfile.write(data)
        self.wfile.flush()
    except (BrokenPipeError, ConnectionResetError):
        st.client_write_failed = True
        raise


def reply_preempted(self, st):
    """O-102 M4: the shared request was cancelled for a local one: an error for the remote
    client (503 before the response started, an SSE error event in a started stream, else a
    closed, incomplete response) and the connection is closed."""
    st.request_error = 'preempted'
    self.close_connection = True
    try:
        if not st.started_response:
            if st.watcher is not None:
                st.watcher.stop()
            self.json_reply(503, {'error': dict(lan_share.PREEMPTED)})
        elif 'text/event-stream' in (st.content_type or ''):
            data = b'data: ' + json.dumps({'error': dict(lan_share.PREEMPTED)}).encode() + b'\n\n'
            write_client(self, st, ('%x\r\n' % len(data)).encode() + data + b'\r\n0\r\n\r\n')
    except OSError:
        pass


def reply_client(self, st, status, data, content_type):
    if st.watcher is not None:
        st.watcher.stop()
    try:
        return self.reply(status, data, content_type)
    except (BrokenPipeError, ConnectionResetError):
        st.client_write_failed = True
        raise


def forward(self, st):
    """Send the prepared request to the engine and stream its answer to the client. Returns
    ctl_queue.REQUEUE when a pre-emptible background request was cut before its first data byte
    (EG1): nothing of it reached the client but SSE comments; the caller admits it again."""
    path, inference = st.path, st.inference
    connection = st.connection
    run = ctl_queue.run_of(st)  # EG1: a running background request, cut by an interactive arrival
    try:
        if run is not None:
            connection.connect()
            run.attach(connection.sock)
            if run.cancelled:
                return ctl_queue.REQUEUE
        if st.preempt is not None:
            # attached before the request is sent: a local request arriving from now on cuts
            # the engine connection (a cancel that came earlier cuts it here at once)
            connection.connect()
            st.preempt.attach(connection.sock)
            if st.preempt.cancelled:
                raise ConnectionAbortedError('preempted')
        connection.request(self.command, st.backend_path, body=st.body, headers=st.backend_headers)
        # the watcher peeks at the client socket (MSG_PEEK), which a TLS socket refuses: on a
        # shared TLS listener it would cancel every request at once, so a vanished TLS client
        # is noticed by the failed (or timed-out) write instead
        if inference and connection.sock is not None and not lan_share.is_tls(self.connection):
            # OXC_REPLAY: a cut arms the replay instead of cancelling the engine (ctl_replay.Guard)
            st.watcher = ctl.ClientDisconnect(self.connection, ctl_replay.start(st, connection.sock)).start()
        response = connection.getresponse()
        if run is not None and (response.status != 200 or 'text/event-stream' not in
                                (response.getheader('Content-Type') or '')) and not run.commit():
            return ctl_queue.REQUEUE  # cut before anything but an SSE head: re-issue
        st.backend_status = response.status
        if inference and response.status >= 400: self.manager.current_prefix_key = None
        if response.status != 200: st.harness_key = None
        tail = b''
        st.content_type = response.getheader('Content-Type', 'application/octet-stream')
        # Static app resources and /props are safe to cache; never cache slots or user responses.
        # Keyed by the path, so only a target without a query is stored (never '/props?x' as '/props').
        cacheable = self.command == 'GET' and response.status == 200 and self.path == path and (
            path == '/props' or path.endswith(('.js', '.css', '.svg', '.ico', '.woff', '.woff2', '.html')))
        if response.getheader('Content-Encoding', '').lower() == 'gzip' and self.command != 'HEAD':
            try:
                data = gzip.decompress(response.read())
            except (OSError, EOFError, ValueError):  # N6: a corrupt gzip body is a 502, not a crash
                return reply_client(self, st, 502, json.dumps({'error': {'message': 'The local backend sent a corrupt '
                                    'compressed response.', 'type': 'local_backend_error'}}).encode(), 'application/json')
            if cacheable: self.manager.cache[path] = (st.content_type, data)
            return reply_client(self, st, response.status, data, st.content_type)
        if cacheable:
            data = response.read()
            self.manager.cache[path] = (st.content_type, data)
            return reply_client(self, st, response.status, data, st.content_type)
        if (response.status >= 400 and self.command != 'HEAD' and 'json' in (st.content_type or '').lower()
                and not path.startswith('/_local/')):
            # v0.2.0 S3: an engine error gets the OpenAI shape (string `code`, `param`); the
            # engine's own message and type are kept
            data = response.read()
            if st.led is not None: st.led.observe(data)
            try: data = json.dumps(openai_api.normalize_error(response.status, json.loads(data))).encode()
            except (ValueError, TypeError, AttributeError): pass
            recorder = ctl_replay.recorder(st)
            if recorder is not None:
                recorder.head(response.status, [])  # never replayable: drops the entry
            return reply_client(self, st, response.status, data, 'application/json')
        shim = None
        if getattr(st, 'shim', None) is not None and response.status == 200:
            # v0.2.0 conformance: chat usage details, response_format check, /v1/responses
            if not st.shim.stream:
                data = response.read()
                if st.led is not None: st.led.observe(data)
                ctl_diag.feed(st, data)
                engine_tail = data[-4096:]
                data = st.shim.body(data)
                add_reply_headers(self, st.shim.headers())
                reply_client(self, st, 200, data, 'application/json')
                if st.harness_key is not None:
                    with self.manager.harness_mu:
                        self.manager.harness_registry.complete(st.harness_key, st.target, *timings_from_tail(engine_tail))
                return None
            shim = st.shim.streamer()
            add_reply_headers(self, st.shim.headers())
        elif st.completion_meta is not None and response.status == 200:
            if not st.completion_meta['stream']:
                data = response.read()
                if st.led is not None: st.led.observe(data)
                try: data = json.dumps(openai_api.completion_response(json.loads(data), st.completion_meta)).encode()
                except ValueError: pass
                return reply_client(self, st, 200, data, 'application/json')
            shim = openai_api.CompletionStream(st.completion_meta)
        # CORS is decided here (openai_api.cors_origins), never by a backend.
        items = [(k, v) for k, v in response.getheaders() if not k.lower().startswith('access-control-')
                 and k.lower() not in HOP_HEADERS and k.lower() != 'x-request-id'
                 and (st.runtime is None or k.lower() not in {'authorization', 'x-api-key', 'api-key', 'set-cookie'})]
        recorder = ctl_replay.recorder(st)
        if recorder is not None:  # before the client head: a prefill cut still records the generation
            recorder.head(response.status, items)
        early = getattr(self, 'early_stream', None)
        if early is not None and early.active():
            # OXC_QUEUE: SSE headers already sent while queued; an engine error is an SSE event
            if response.status != 200:
                return reply_client(self, st, response.status, response.read(), st.content_type)
            early.engine_started = True
        else:
            try:
                self.send_response(response.status)
                for k, v in items: self.send_header(k, v)
                for k, v in self.cors_reply_headers(): self.send_header(k, v)
                self.send_header('Cache-Control', 'no-store')
                self.send_header('Transfer-Encoding', 'chunked')
                self.end_headers()
                if run is not None and early is not None:
                    early.early = early.engine_started = True  # EG1: the head is out (re-queue keeps it)
            except (BrokenPipeError, ConnectionResetError):
                st.client_write_failed = True
                if recorder is None or not recorder.armed or not recorder.cut():
                    raise
                # OXC_REPLAY: the client left during prefill; keep reading for a retry
        st.started_response = True
        if self.command != 'HEAD':
            while chunk := response.read1(65536):
                if run is not None and not run.gate(chunk):
                    return ctl_queue.REQUEUE  # EG1: cut before this first data byte; dropped
                if st.harness_key is not None: tail = (tail + chunk)[-4096:]
                if st.led is not None: st.led.observe(chunk)
                ctl_diag.feed(st, chunk)
                if shim is not None:
                    chunk = shim.feed(chunk)
                    if not chunk: continue
                if recorder is not None and recorder.relay(self, st, chunk, write_client):
                    continue
                write_client(self, st, ('%x\r\n' % len(chunk)).encode() + chunk + b'\r\n')
            if shim is not None and (chunk := shim.finish()):
                if recorder is None or not recorder.relay(self, st, chunk, write_client):
                    write_client(self, st, ('%x\r\n' % len(chunk)).encode() + chunk + b'\r\n')
            if st.watcher is not None:
                st.watcher.stop()
            if run is not None and run.cancelled and not st.client_gone():
                return ctl_queue.REQUEUE  # EG1: the engine stopped for the cut, before any data
            if st.preempted():
                # the engine connection was cut for a local request: not a complete answer
                reply_preempted(self, st)
            elif st.request_process is not None and st.request_process.poll() is not None:
                # Headers already escaped; a closed, incomplete response is
                # honest, whereas a final chunk would disguise engine death.
                st.request_error = 'backend_died'
                self.close_connection = True
            else:
                if recorder is None or not recorder.done(st):  # a cut client gets no final chunk
                    write_client(self, st, b'0\r\n\r\n')
                if st.harness_key is not None:
                    with self.manager.harness_mu:
                        self.manager.harness_registry.complete(st.harness_key, st.target, *timings_from_tail(tail))
    except (BrokenPipeError, ConnectionResetError):
        if run is not None and run.cancelled and not st.client_gone():
            return ctl_queue.REQUEUE
        if st.client_gone():
            self.close_connection = True
        elif st.preempted():
            reply_preempted(self, st)
        else:
            backend_failed(self, st)
    except (OSError, http.client.HTTPException) as error:
        if run is not None and run.cancelled and not st.client_gone():
            return ctl_queue.REQUEUE
        if st.client_gone():
            self.close_connection = True
        elif st.preempted():
            reply_preempted(self, st)
        else:
            if inference: self.manager.current_prefix_key = None
            if isinstance(error, http.client.RemoteDisconnected) or (
                    st.request_process is not None and st.request_process.poll() is not None):
                backend_failed(self, st)
            elif not st.started_response:
                self.json_reply(503, {'error': {'message': 'Local backend connection failed.', 'type': 'local_backend_error'}})
            else:
                self.close_connection = True
