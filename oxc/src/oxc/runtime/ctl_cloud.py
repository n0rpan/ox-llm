"""Proxy stage for a remote catalog model (Wave D: D1/D2).

Called from `ctl_proxy.proxy_request` after the request body is prepared and the ledger row
is open, instead of local admission.  The request never touches the local engine, the prefix
store or the local model lease.
"""
import http.client
import json
if __package__:
    from .ctl_proxy_prep import HOP_HEADERS
    from .ctl_seams import ctl
    from . import cloud, openai_api
    from . import cloud_anthropic, cloud_chatgpt, cloud_openai
else:
    from ctl_proxy_prep import HOP_HEADERS
    from ctl_seams import ctl
    import cloud
    import openai_api
    import cloud_anthropic
    import cloud_chatgpt
    import cloud_openai

MAX_STREAM_BYTES = 64 * 1024 * 1024


def _close_row(st, status, error=None):
    """Finish the ledger row and remember a write failure so cloud admission fails closed."""
    if st.led is None:
        return
    try:
        st.led.finish(status, error)
    except Exception:
        # An append may have happened before an exception; never retry an ambiguous write.
        cloud.mark_ledger_failure(st.led.row.get('cost_usd'))
        return
    if getattr(st.led, 'written', True) is False:
        cloud.mark_ledger_failure(st.led.row.get('cost_usd'))


def _reply(self, st, status, message, code, error=None, param='model'):
    if st.led is not None:
        # A refusal is a remote row with no spend, never a row that under-counts the cap.
        st.led.row.setdefault('cost_usd', 0.0)
        st.led.row.setdefault('settled', 'not_forwarded')
    _close_row(st, status, error or code)
    return self.json_reply(status, {'error': {'message': message, 'type': 'permission_error' if status == 403 else
                                              ('rate_limit_error' if status == 429 else 'cloud_error'),
                                              'param': param, 'code': code}})


def _finish(self, st, item, meter, status, error=None, hold=None, secret=None, failed=False,
            cancelled=None, refused=False):
    """Record one forwarded request.  Missing usage charges the reserved worst case.

    ``cancelled`` = (request_bytes, streamed_bytes) for a client cancel: settled at actual usage
    or an estimate of what was streamed."""
    if hold is not None and hold.accounted:
        return  # Accounting was attempted; any failure already closed cloud admission.
    row = meter.row(item, secret)
    if cancelled is not None:
        # OpenAI/OpenRouter report usage only in the closing chunk, so any usage seen is final.
        cloud.cancelled_cost(row, hold, item, *cancelled, usage_final=True)
    else:
        cloud.conservative_cost(row, hold, failed=failed, refused=refused)
    if st.led is not None:
        st.led.row.update(row)
        _close_row(st, status, error)
    if hold is not None:
        hold.accounted = True
        hold.settle()


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
    return json.dumps(cloud.redact_value(parsed, secret), ensure_ascii=False).encode()


def _completed_response(data):
    """The final response object of a Responses SSE body, as JSON bytes."""
    decoder = cloud.SSEDecoder()
    events = decoder.feed(data) + decoder.finish()
    for _, raw in reversed(events):
        try:
            value = json.loads(raw)
        except ValueError:
            continue
        if isinstance(value, dict) and value.get('type') in ('response.completed', 'response.incomplete'):
            return json.dumps(value.get('response'), ensure_ascii=False).encode()
    raise cloud.CloudError('The ChatGPT stream ended before completion.', 502, 'cloud_stream_error')


def forward(self, st):
    """Route one remote chat request to its upstream and stream the answer back."""
    item = st.cloud_entry
    value = st.api_value
    if st.led is not None:
        st.led.row.update({'remote': True, 'remote_provider': item['provider']})
    if st.remote:
        return _reply(self, st, 403, 'Cloud models are not available to LAN-share requests.', 'cloud_not_shared')
    if cloud.offline():
        return _reply(self, st, 403, 'Cloud models are unavailable in offline mode.', 'offline')
    try:
        cloud.validate_remote_request(st.payload, item)
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code, param=error.param)
    account = None
    try:
        if item['provider'] == 'chatgpt':
            try:
                key, account = cloud_chatgpt.credentials(cloud.secrets_file(value), value)
            except cloud_chatgpt.LoginError as error:
                raise cloud.CloudError('ChatGPT sign-in unavailable: %s.' % error, 503,
                                       'cloud_credentials_unavailable') from None
        else:
            key = cloud.secret(item['provider'], value)
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code, 'cloud_credentials_unavailable', error.param)
    if item['provider'] == 'anthropic':
        return cloud_anthropic.forward(self, st, item, key)
    # Continuations of the subscription provider belong to the account, not to one access
    # token: a refresh between a tool call and its result must not break the loop.
    scope = 'chatgpt-account:' + account if account else None
    try:
        body = cloud.upstream_payload(st.payload, item, key, scope)
        data = json.dumps(body, ensure_ascii=False, allow_nan=False).encode()
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code, param=error.param)
    except (TypeError, ValueError, UnicodeError):
        return _reply(self, st, 400, 'Invalid cloud request encoding.', 'invalid_request')
    try:
        hold = cloud.reserve(value, item, body)
    except cloud.CloudError as error:
        return _reply(self, st, error.status, str(error), error.code, error.code, param=error.param)
    responses = (cloud_openai.ResponsesStream(item, st.payload.get('messages'), key, scope)
                 if item['provider'] in ('openai', 'chatgpt') and cloud_openai.endpoint(item) == '/responses'
                 else None)
    # The subscription backend always streams; a client that did not ask for a stream gets
    # the final response assembled from it.
    collect = item['provider'] == 'chatgpt' and st.payload.get('stream') is not True
    meter = cloud.Meter()
    connection = None
    started = False
    try:
        connection, path, headers = cloud.connection(item, key, account)
        connection.request('POST', path, body=data, headers=headers)
        response = connection.getresponse()
        st.backend_status = response.status
        content_type, full_type = _safe_headers(response)
        if item['provider'] == 'chatgpt':
            cloud_chatgpt.observe(response.getheaders())
        if response.status >= 400:
            payload = _error_body(response.status, response.read(cloud.MAX_ERROR_BYTES), key)
            _finish(self, st, item, meter, response.status, 'cloud_upstream_error', hold, key, failed=True,
                    refused=cloud.clean_refusal(response.status))
            return self.reply(response.status, payload, 'application/json')
        if not collect and (body.get('stream') is True or 'text/event-stream' in full_type.lower()):
            self.send_response(response.status)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Transfer-Encoding', 'chunked')
            for name, header_value in self.cors_reply_headers():
                self.send_header(name, header_value)
            self.end_headers()
            started = True
            total = 0
            sent = [0]
            streamer = cloud.CloudStream(item['id'], key=key)
            redactor = cloud.StreamRedactor(key)

            def write(data):
                if not data:
                    return
                safe = redactor.feed(data)
                if not safe:
                    return
                if st.led is not None:
                    st.led.observe(safe)
                self.wfile.write(('%x\r\n' % len(safe)).encode() + safe + b'\r\n')
                self.wfile.flush()
                sent[0] += len(safe)

            try:
                while chunk := response.read1(65536):
                    total += len(chunk)
                    if total > MAX_STREAM_BYTES:
                        st.request_error = 'cloud_response_too_large'
                        self.close_connection = True
                        break
                    if responses is not None:
                        chunk = responses.feed(chunk)
                    meter.feed(chunk, sse=True)
                    write(streamer.feed(chunk))
                if responses is not None:
                    tail = responses.finish()
                    meter.feed(tail, sse=True)
                    write(streamer.feed(tail))
                write(streamer.finish())
                meter.finish_sse()
                tail = redactor.finish()
                if tail:
                    if st.led is not None:
                        st.led.observe(tail)
                    self.wfile.write(('%x\r\n' % len(tail)).encode() + tail + b'\r\n')
                self.wfile.write(b'0\r\n\r\n')
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                st.client_write_failed = True
                st.request_error = 'client_disconnected'
                self.close_connection = True
                _finish(self, st, item, meter, 499, 'cancelled', hold, key,
                        cancelled=(len(data), sent[0], cloud.elapsed_since(hold)))
                return
            _finish(self, st, item, meter, response.status, st.request_error, hold, key,
                    failed=st.request_error is not None)
            return
        payload = response.read(MAX_STREAM_BYTES + 1)
        if len(payload) > MAX_STREAM_BYTES:
            _finish(self, st, item, meter, 502, 'cloud_response_too_large', hold, key, failed=True)
            return self.json_reply(502, {'error': {'message': 'The cloud provider sent an oversized response.',
                                                   'type': 'cloud_error', 'code': 'cloud_response_too_large'}})
        if response.length is not None and response.length > 0:
            raise http.client.IncompleteRead(payload, response.length)
        if collect:
            payload = _completed_response(payload)
        if responses is not None:
            payload = json.dumps(cloud_openai.response(json.loads(payload), item,
                                 st.payload.get('messages'), key, scope), ensure_ascii=False).encode()
        meter.feed(payload)
        try:
            parsed = json.loads(payload)
            if item['provider'] in ('openai', 'chatgpt'):
                parsed = cloud.redact_value(parsed, key)
            payload = cloud.normalize_response(parsed, item['id'])
            payload = json.dumps(payload, ensure_ascii=False).encode()
        except (ValueError, TypeError):
            pass  # an upstream body that is not JSON is relayed as it arrived
        payload = cloud.redact(payload, key)
        if st.led is not None:
            st.led.observe(payload)
        # the row is written before the reply goes out, so a client that sees the answer can rely on
        # the next request already being gated by a failed ledger write
        _finish(self, st, item, meter, response.status, hold=hold, secret=key)
        try:
            self.reply(response.status, payload, 'application/json')
        except (BrokenPipeError, ConnectionResetError):
            st.client_write_failed = True
            self.close_connection = True
    except cloud.CloudError as error:
        _finish(self, st, item, meter, error.status, error.code, hold, key, failed=True)
        if started:
            self.close_connection = True
        else:
            self.json_reply(error.status, {'error': {'message': cloud.redact(str(error), key),
                                                   'type': 'cloud_error', 'param': error.param,
                                                   'code': error.code}})
    except Exception:  # every transport/protocol failure retains its conservative charge
        _finish(self, st, item, meter, 503, 'cloud_unavailable', hold, key, failed=True)
        if not started:
            self.json_reply(503, {'error': {'message': 'The cloud provider is unavailable.',
                                            'type': 'cloud_error', 'param': 'model', 'code': 'cloud_unavailable'}})
        else:
            self.close_connection = True
    finally:
        if hold is not None and not hold.settled:
            try:
                _finish(self, st, item, meter, 503, 'cloud_interrupted', hold, key, failed=True)
            except Exception:
                # If settlement itself fails, no subsequent request may spend past it.
                cloud.mark_ledger_failure(hold.reserved)
            finally:
                hold.settle()
        if connection is not None:
            try:
                connection.close()
            except OSError:
                pass
