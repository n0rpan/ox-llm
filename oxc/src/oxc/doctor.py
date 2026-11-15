"""Read hardware capabilities without loading models or changing the system."""
import os
import platform
import shutil
import subprocess
from pathlib import Path
from . import power
from .platform_support import detect

CODE_ENDPOINT='http://127.0.0.1:8092'

def tool_checks(path=None,home=None,run=subprocess.run):
    """Read-only checks for the coding agent: bubblewrap, oxa and the controller endpoint the code
    launcher uses (reported as a constant; no network call). Never raises."""
    search=os.environ.get('PATH',os.defpath) if path is None else path
    home=Path(home) if home is not None else Path(os.environ.get('HOME') or Path.home())
    bwrap=shutil.which('bwrap',path=search)
    installed=home/'.local/bin/oxa'
    oxa=str(installed) if installed.is_file() and os.access(installed,os.X_OK) else shutil.which('oxa',path=search)
    version=None;error=None
    if oxa:
        try:
            done=run([oxa,'--version'],stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
                     text=True,timeout=5)
            text=(done.stdout or done.stderr or '').strip()
            if done.returncode==0 and text:version=text.splitlines()[0][:200]
            else:error='oxa --version exited '+str(done.returncode)
        except subprocess.TimeoutExpired:error='oxa --version timed out after 5 s'
        except (OSError,subprocess.SubprocessError,ValueError) as exc:error=type(exc).__name__+': '+str(exc)[:200]
    return {'bwrap':{'present':bwrap is not None,'path':bwrap,
                     'note':None if bwrap else "oxa's bash tool refuses to run without bubblewrap (bwrap)"},
            'oxa':{'present':oxa is not None,'path':oxa,'version':version,'error':error},
            'code_endpoint':CODE_ENDPOINT}

def inspect(*,path=None,home=None,run=subprocess.run):
    memory={}
    for line in Path('/proc/meminfo').read_text().splitlines():
        if line.startswith(('MemTotal:','MemAvailable:')):
            memory[line.split(':',1)[0]]=int(line.split()[1])*1024
    driver_names=set()
    for driver in Path('/sys/class/drm').glob('card*/device/driver'):
        try:driver_names.add(driver.resolve().name)
        except OSError:pass
    npu=Path('/dev/fastrpc-cdsp')
    heap=Path('/dev/dma_heap/system')
    return {'schema_version':1,'architecture':platform.machine(),'logical_cpus':os.cpu_count(),
            'memory_bytes':memory,'drm_driver_names':sorted(driver_names),
            'hexagon_device_present':npu.exists(),
            'hexagon_device_accessible':os.access(npu,os.R_OK|os.W_OK),
            'system_dma_heap_present':heap.exists(),
            'system_dma_heap_readable':os.access(heap,os.R_OK),
            'power':power.snapshot(),'platform_support':detect(),'heavy_work_started':False,
            'coding_agent':tool_checks(path,home,run)}
