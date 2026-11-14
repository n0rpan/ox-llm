#!/usr/bin/env python3
"""Install a staged release, roll back, or smoke-check the loopback controller.

Lifecycle operations retain release_update's seal, preimage, service identity and automatic
rollback checks. The smoke check uses a tiny local-model chat, marks usage as lab, and unloads
only a model it loaded. No fault injection, process killing or cache corruption.
"""
import argparse
import http.client
import json
from pathlib import Path
import sys
import urllib.parse

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'src'))
from snapdragon_ai_lab.runtime import release_update


class SmokeError(Exception):
    pass


def controller_url(value):
    try:
        parts = urllib.parse.urlsplit(value)
        if (parts.scheme != 'http' or parts.hostname not in ('127.0.0.1', 'localhost', '::1')
                or parts.username or parts.password or parts.query or parts.fragment
                or parts.path not in ('', '/') or not 0 < (parts.port or 80) < 65536):
            raise ValueError()
    except ValueError:
        raise argparse.ArgumentTypeError('an HTTP loopback controller URL is required') from None
    return value.rstrip('/')


def request(controller, path, payload=None, *, timeout=10):
    parts = urllib.parse.urlsplit(controller)
    connection = http.client.HTTPConnection(parts.hostname, parts.port or 80, timeout=timeout)
    try:
        data = json.dumps(payload).encode() if payload is not None else None
        headers = {'Content-Type': 'application/json', **release_update.LAB_HEADER}
        connection.request('POST' if data is not None else 'GET', path, body=data, headers=headers)
        reply = connection.getresponse()
        raw = reply.read((1 << 20) + 1)
        if len(raw) > 1 << 20:
            raise SmokeError('The controller response is too large.')
        return reply.status, json.loads(raw)
    except (OSError, ValueError, http.client.HTTPException):
        raise SmokeError('The loopback controller did not return a valid response.') from None
    finally:
        connection.close()


def smoke(controller):
    code, status = request(controller, '/_local/status')
    if code != 200 or not isinstance(status, dict) or status.get('state') not in ('unloaded', 'ready', 'loaded'):
        raise SmokeError('The controller is not ready for a smoke check.')
    code, models = request(controller, '/v1/models')
    rows = models.get('data') if isinstance(models, dict) else None
    if code != 200 or not isinstance(rows, list):
        raise SmokeError('The controller model list is unavailable.')
    model = status.get('model') or status.get('selected_model') or status.get('selected_alias')
    if not isinstance(model, str) or not model:
        raise SmokeError('Select a listed local model before running the smoke check.')
    selected = next((row for row in rows if isinstance(row, dict) and row.get('id') == model), None)
    caps = selected.get('capabilities', {}) if selected is not None else {}
    if (selected is None or not isinstance(caps, dict)
            or selected.get('remote', False) is not False or caps.get('remote', False) is not False):
        raise SmokeError('Select a listed local model before running the smoke check.')
    loaded = status['state'] in ('ready', 'loaded') or bool(status.get('loaded_model'))
    try:
        code, reply = request(controller, '/v1/chat/completions', {
            'model': model, 'temperature': 0, 'max_tokens': 8, 'stream': False,
            'messages': [{'role': 'user', 'content': 'Reply with one word: ready'}]}, timeout=180)
        choices = reply.get('choices') if isinstance(reply, dict) else None
        choice = choices[0] if isinstance(choices, list) and choices else None
        message = choice.get('message') if isinstance(choice, dict) else None
        content = message.get('content') if isinstance(message, dict) else None
        if code != 200 or not isinstance(content, str) or not content.strip():
            raise SmokeError('The smoke chat did not answer.')
    finally:
        if not loaded:
            code, _ = request(controller, '/_local/unload', {}, timeout=120)
            if code != 200:
                raise SmokeError('The smoke check could not unload its model.')
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    commands = parser.add_subparsers(dest='command', required=True)
    install = commands.add_parser('install', help='install a local staged release with rollback protection')
    install.add_argument('stage', type=Path)
    commands.add_parser('rollback', help='restore the previous installed release')
    commands.add_parser('smoke', help='one tiny local-model chat; no fault injection')
    parser.add_argument('--controller', type=controller_url, default=release_update.CHAT)
    args = parser.parse_args(argv)
    try:
        if args.command == 'install':
            return release_update.update(args.stage, chat=args.controller)
        if args.command == 'rollback':
            return release_update.rollback(chat=args.controller)
        result = smoke(args.controller)
        print('PASS smoke')
        return result
    except SmokeError as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
