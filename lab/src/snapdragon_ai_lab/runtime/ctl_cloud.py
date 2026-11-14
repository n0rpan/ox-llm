"""Proxy stage for a remote catalog model (Wave D: D1/D2).

Called from `ctl_proxy.proxy_request` after the request body is prepared and the ledger row
is open, instead of local admission.  The request never touches the local engine, the prefix
store or the local model lease.
"""
import json
import threading
if __package__:
    from .ctl_proxy_prep import HOP_HEADERS
    from .ctl_seams import ctl
    from . import cloud, openai_api
else:
    from ctl_proxy_prep import HOP_HEADERS
    from ctl_seams import ctl
    import cloud
    import openai_api

MAX_STREAM_BYTES = 64 * 1024 * 1024
_budget_lock = threading.Lock()


def _reply(self, st, status, message, code, error=None):
    if st.led is not None:
        st.led.finish(status, error or code)
    return self.json_reply(status, {'error': {'message': message, 'type': 'permission_error' if status == 403 else
                                              ('rate_limit_error' if status == 429 else 'cloud_error'),
                                              'param': 'model', 'code': code}})


def _finish(self, st, item, meter, status, error=None):
    if st.led is not None:
        st.led.row.update(meter.row(item))
        st.led.finish(status, error)


def _safe_headers(response):
    """Only the response's content type belongs to the client; never cookies or credentials."""
    content_type = response.getheader('Content-Type') or 'application/json'
    return content_type.split(';', 1)[0].strip().lower() or 'application/json', content_type


def _error_body(status, data, secret):
    """An upstream error in the OpenAI shape, with the key (if it ever appears) removed."""
    text = cloud.redact(data[:cloud.MAX_ERROR_BYTES], secret)
    try:
        parsed = json.loads(text)
    except ValueError:
        parsed = {'error': {'message': 'The cloud provider refused the request.',
                            'type': 'cloud_error', 'code': 'cloud_upstream_error'}}
    try:
        parsed = openai_api.normalize_error(status, parsed)
    except Exception:
        parsed = {'error': {'message': 'The cloud provider refused the request.',
                            'type': 'cloud_error', 'code': 'cloud_upstream_error'}}
    return json.dumps(parsed, ensure_ascii=False).encode()


def forward(self, st):
    """Route one remote chat request to its upstream and stream the answer back."""
    item = st.cloud_entry
    value = st.api_value
    if st.led is not None:
        st.led.row.update({'remote': True, 'remote_provider': item['provider']})
    if cloud.offline():
        return _reply(self, st, 403, 'Cloud models are unavailable in offline mode.', 'offline')
    cap, spend = cloud.budget(value)
    if cap is None:
        return _reply(self, st, 403, 'Cloud models need a per-day USD budget (`cloud.daily_budget_usd`).',
                      'budget_not_configured')
    if spend >= cap:
        return _reply(self, st, 429, 'The daily cloud budget is exhausted.', 'budget_exceeded')
    try:
        key = cloud.secret(item['provider'], value)
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code, 'cloud_credentials_unavailable')
    try:
        body = cloud.remote_payload(st.payload, item)
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code)
    if body.get('stream') is True:
        options = body.get('stream_options') if isinstance(body.get('stream_options'), dict) else {}
        body['stream_options'] = {**options, 'include_usage': True}
    data = json.dumps(body, ensure_ascii=False).encode()
    meter = cloud.Meter()
    connection = None
    started = False
    try:
        connection, path, headers = cloud.connection(item, key)
        connection.request('POST', path, body=data, headers=headers)
        response = connection.getresponse()
        st.backend_status = response.status
        content_type, full_type = _safe_headers(response)
        if response.status >= 400:
            payload = _error_body(response.status, response.read(cloud.MAX_ERROR_BYTES), key)
            _finish(self, st, item, meter, response.status, 'cloud_upstream_error')
            return self.reply(response.status, payload, 'application/json')
        if body.get('stream') is True or 'text/event-stream' in full_type.lower():
            self.send_response(response.status)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Transfer-Encoding', 'chunked')
            for name, header_value in self.cors_reply_headers():
                self.send_header(name, header_value)
            self.end_headers()
            started = True
            total = 0
            try:
                while chunk := response.read1(65536):
                    total += len(chunk)
                    if total > MAX_STREAM_BYTES:
                        st.request_error = 'cloud_response_too_large'
                        self.close_connection = True
                        break
                    meter.feed(chunk, sse=True)
                    if st.led is not None:
                        st.led.observe(chunk)
                    self.wfile.write(('%x\r\n' % len(chunk)).encode() + chunk + b'\r\n')
                    self.wfile.flush()
                self.wfile.write(b'0\r\n\r\n')
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                st.client_write_failed = True
                st.request_error = 'client_disconnected'
                self.close_connection = True
            _finish(self, st, item, meter, response.status, st.request_error)
            return
        payload = response.read(MAX_STREAM_BYTES + 1)
        if len(payload) > MAX_STREAM_BYTES:
            _finish(self, st, item, meter, 502, 'cloud_response_too_large')
            return self.json_reply(502, {'error': {'message': 'The cloud provider sent an oversized response.',
                                                   'type': 'cloud_error', 'code': 'cloud_response_too_large'}})
        meter.feed(payload)
        if st.led is not None:
            st.led.observe(payload)
        payload = cloud.redact(payload, key)
        self.reply(response.status, payload, 'application/json')
        _finish(self, st, item, meter, response.status)
    except cloud.CloudError as error:
        _finish(self, st, item, meter, error.status, error.code)
        self.json_reply(error.status, {'error': {'message': str(error), 'type': 'cloud_error',
                                                 'param': 'model', 'code': error.code}})
    except (OSError, ValueError, json.JSONDecodeError) as error:
        detail = cloud.redact(str(error), key)
        _finish(self, st, item, meter, 503, 'cloud_unavailable')
        if not started:
            self.json_reply(503, {'error': {'message': 'The cloud provider is unavailable. ' + detail[:200],
                                            'type': 'cloud_error', 'param': 'model', 'code': 'cloud_unavailable'}})
        else:
            self.close_connection = True
    finally:
        if connection is not None:
            try:
                connection.close()
            except OSError:
                pass
