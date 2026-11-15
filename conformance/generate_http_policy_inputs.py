"""Regenerate the Rust test inputs from the shared synthetic Python cases."""
import json
import sys
from pathlib import Path

from http_policy_cases import CASES


if __name__ == '__main__':
    inputs = {name: {key: list(value) if isinstance(value, bytes) else value
                     for key, value in case.items()} for name, case in CASES.items()}
    path = Path(__file__).with_name('goldens') / 'http-policy-inputs.json'
    data = '{\n' + ',\n'.join(json.dumps(k) + ':' + json.dumps(v, sort_keys=True, separators=(',', ':')) for k,v in inputs.items()) + '\n}\n'
    check = sys.argv[1:] == ['--check']
    if check:
        assert path.read_text() == data, 'policy input fixture drift'
    else:
        path.write_text(data)
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'oxc/src'))
    from oxc.runtime.openai_api import normalize_error
    cases = [(status, {'error': 'Synthetic error'}) for status in
             (400,401,403,404,408,409,411,413,422,429,499,500,501,502,503,504)]
    cases += [(400, {'error': {'message':'context size exceeded', 'extra':7}}),
              (404, {'error': {'message':'Unknown model', 'param':None}}),
              (503, {'error': {'message':'Synthetic', 'type':'custom_type','code':'custom_code','param':'field'}}),
              (200, {'error':'unchanged'}), (500, {'other':'unchanged'})]
    data = ''.join(json.dumps({'status':status,'input':value,'expected':normalize_error(status,value)},
                  sort_keys=True,separators=(',',':')) + '\n' for status,value in cases)
    path = path.with_name('http-policy-errors.jsonl')
    if check:
        assert path.read_text() == data, 'error fixture drift'
    else:
        path.write_text(data)
