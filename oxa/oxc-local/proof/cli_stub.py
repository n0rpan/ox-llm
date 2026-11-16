"""Existing helper-only synthetic native-local CLI proof, with portable input paths.

Usage: python3 -I cli_stub.py OXA OXE SYNTHETIC_GGUF [print|rpc|tui] [STRACE]
Only use the repository's generated synthetic GGUF. This is not real-model acceptance.
Python orchestrates the proof outside the product process tree.
"""
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
if not 4 <= len(sys.argv) <= 6:
    raise SystemExit(__doc__)
binary=pathlib.Path(sys.argv[1]).resolve(strict=True)
engine=pathlib.Path(sys.argv[2]).resolve(strict=True)
model=pathlib.Path(sys.argv[3]).resolve(strict=True)
mode=sys.argv[4] if len(sys.argv)>4 else 'print'
if mode not in ('print','rpc','tui'):
    raise SystemExit(__doc__)
strace=sys.argv[5] if len(sys.argv)>5 else shutil.which('strace')
with tempfile.TemporaryDirectory(prefix='native-local-cli-') as raw:
    tmp=pathlib.Path(raw);home=tmp/'home';work=tmp/'project';work.mkdir();settings=home/'.oxa/agent/settings.json';settings.parent.mkdir(parents=True)
    settings.write_text(json.dumps({'defaultProjectTrust':'always','retry':{'enabled':False},'oxa':{'engine':{'inProcessLocal':True},'local':{'binary':str(engine),'models':{'synthetic':{'model':str(model),'arguments':['--engine','stub','--ctx-size','4096','--reasoning','off','--stub-reply','</think>Synthetic task complete.']}},'limits':{'synthetic':{'context_window':4096,'max_output':64,'allocation_budget':{'kv_bytes_per_position':1,'scratch_bytes':0,'mtp_state_bytes':0,'margin_bytes':1044480}}}}}}))
    files={'membership':'0::/engine\n','meminfo':'MemAvailable: 8388608 kB\nSwapFree: 0 kB\n','pressure':'full avg10=0\n','vmstat':'oom_kill 0\n','cgroup/engine/memory.events':'oom 0\noom_kill 0\noom_group_kill 0\n','cgroup/engine/memory.swap.current':'0\n','cgroup/engine/memory.swap.max':'0\n','cgroup/engine/pids.events':'max 0\n'}
    for name,value in files.items():
        p=tmp/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(value)
    markers=tmp/'markers';markers.mkdir()
    for name in ['python','python3','oxc']:
        p=markers/name;p.write_text('#!/bin/sh\n: > "'+str(tmp/'unexpected-interpreter')+'"\nexit 97\n');p.chmod(0o755)
    args=['bwrap','--unshare-user','--unshare-pid','--as-pid-1','--unshare-net','--ro-bind','/','/','--tmpfs','/tmp','--proc','/proc','--dev','/dev','--bind',raw,raw,'--ro-bind',str(model),str(model),'--ro-bind',str(binary),str(binary),'--ro-bind',str(engine),str(engine),'--ro-bind',str(tmp/'cgroup'),'/sys/fs/cgroup','--ro-bind',str(tmp/'membership'),'/proc/1/cgroup','--ro-bind',str(tmp/'meminfo'),'/proc/meminfo','--ro-bind',str(tmp/'pressure'),'/proc/pressure/memory','--ro-bind',str(tmp/'vmstat'),'/proc/vmstat','--chdir',str(work),'--',str(binary),'-p','--offline','--no-session','--no-tools','--no-mcp','--no-extensions','--no-skills','--no-prompt-templates','--no-context-files','--model','synthetic','Synthetic local CLI task']
    if mode=='tui':
        import time,shlex
        args=args[:args.index('--')+1]+[str(binary),'--offline','--no-session','--no-tools','--no-mcp','--no-extensions','--no-skills','--no-prompt-templates','--no-context-files','--model','synthetic']
        server='native-local-'+tmp.name
        tmux=['tmux','-L',server,'-f','/dev/null']
        testenv={'PATH':'/usr/bin:/bin','HOME':str(home),'TERM':'xterm-256color'}
        def mux(*cmd): return subprocess.run(tmux+list(cmd),env=testenv,capture_output=True,text=True,check=True).stdout
        try:
            mux('new-session','-d','-s','proof','-x','120','-y','32',shlex.join(args))
            deadline=time.monotonic()+15
            while time.monotonic()<deadline:
                pane=mux('capture-pane','-p','-t','proof:0')
                if 'synthetic' in pane.lower(): break
                time.sleep(.1)
            mux('send-keys','-t','proof:0','-l','Synthetic local TUI task')
            mux('send-keys','-t','proof:0','Enter')
            deadline=time.monotonic()+15
            while time.monotonic()<deadline:
                pane=mux('capture-pane','-p','-t','proof:0','-S','-100')
                if 'Synthetic task complete.' in pane: break
                time.sleep(.1)
            else: raise AssertionError('TUI did not render streamed stub reply: '+pane)
            mux('send-keys','-t','proof:0','C-d')
            time.sleep(.5)
            assert not (tmp/'unexpected-interpreter').exists()
            print('PASS: optimized real TUI in tmux selects native local and renders authenticated stub reply offline; interpreter markers untouched')
        finally:
            subprocess.run(tmux+['kill-server'],env=testenv,capture_output=True)
        raise SystemExit(0)
    if mode=='rpc':
        import time,select
        args=args[:args.index('--')+1]+[str(binary),'--mode','rpc','--offline','--no-session','--no-tools','--no-mcp','--no-extensions','--no-skills','--no-prompt-templates','--no-context-files','--model','synthetic']
        child=subprocess.Popen(args,env={'PATH':str(markers)+':/usr/bin:/bin','HOME':str(home)},stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,bufsize=1)
        def descendants(pid):
            found=[]
            try: children=pathlib.Path(f'/proc/{pid}/task/{pid}/children').read_text().split()
            except FileNotFoundError: return found
            for c in children: found.append(int(c));found.extend(descendants(int(c)))
            return found
        def ownpid():
            for pid in [child.pid]+descendants(child.pid):
                try:
                    if pathlib.Path(f'/proc/{pid}/comm').read_text().strip()=='oxa': return pid
                except FileNotFoundError: pass
            raise AssertionError('CLI process missing')
        def sample(label):
            pid=ownpid();lines=pathlib.Path(f'/proc/{pid}/status').read_text().splitlines()
            print(label, ', '.join(l for l in lines if l.startswith(('VmRSS:','Threads:'))))
        def send(v): child.stdin.write(json.dumps(v)+'\n');child.stdin.flush()
        def read_until(predicate):
            deadline=time.monotonic()+15
            while time.monotonic()<deadline:
                if child.poll() is not None: raise AssertionError(child.stderr.read())
                if not select.select([child.stdout],[],[],.1)[0]: continue
                line=child.stdout.readline()
                v=json.loads(line)
                if predicate(v): return v
            raise AssertionError('RPC deadline')
        try:
            send({'type':'get_state','id':'cold'})
            read_until(lambda v:v.get('id')=='cold');sample('idle')
            send({'type':'prompt','id':'task','message':'Synthetic local RPC task'})
            read_until(lambda v:'text_delta' in json.dumps(v));sample('streaming')
            read_until(lambda v:v.get('type')=='agent_end');sample('after-call')
            pids=descendants(child.pid)
            for pid in [child.pid]+pids:
                try: assert pathlib.Path(f'/proc/{pid}/comm').read_text().strip() not in ('python','python3','oxc')
                except FileNotFoundError: pass
            child.stdin.close();child.wait(timeout=5)
            assert child.returncode==0
            assert not (tmp/'unexpected-interpreter').exists()
            assert not list((home/'.oxa/agent/local-engine').iterdir())
            print('PASS: actual native offline RPC streams and cleans up; no interpreter descendant')
        finally:
            if child.poll() is None: child.kill();child.wait()
        raise SystemExit(0)
    trace = tmp/'execve.log'
    if strace is None: raise SystemExit('print mode needs the real strace executable')
    args = [strace,'-f','-e','trace=execve','-o',str(trace)] + args
    result=subprocess.run(args,env={'PATH':str(markers)+':/usr/bin:/bin','HOME':str(home)},capture_output=True,text=True,timeout=30)
    if result.returncode: print(result.stderr);raise SystemExit(result.returncode)
    assert result.stdout.strip(), 'no streamed reply; '+result.stderr
    import re
    executables = re.findall(r'execve\("([^\"]+)"', trace.read_text())
    assert not any(pathlib.Path(p).name in ('python','python3','oxc') for p in executables)
    assert str(binary) in executables and str(engine) in executables
    print('PASS: execve trace includes actual oxa and owned stub, with no Python/controller executable')
    assert not (tmp/'unexpected-interpreter').exists()
    auth=home/'.oxa/agent/local-engine';assert auth.exists() and not list(auth.iterdir()),'auth cleanup missing'
    print('PASS: actual oxa offline print selects native local factory, streams authenticated stub reply and cleans child auth; interpreter markers untouched')
