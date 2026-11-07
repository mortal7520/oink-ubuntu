#!/usr/bin/env python3
"""Launch only isolated Xvfb/Pulse test instances; stop via the product's IPC."""
import json, os, pathlib, signal, subprocess, tempfile, time
root = pathlib.Path(__file__).resolve().parents[2]
directory = pathlib.Path(tempfile.mkdtemp(prefix="oink-desktop-", dir=os.environ.get("OINK_TEST_ARTIFACT_ROOT")))
directory.chmod(0o700)
env = os.environ | {"OINK_HOME":str(directory), "OINK_PORT":"0", "XDG_SESSION_TYPE":"x11",
    "GDK_BACKEND":"x11", "LIBGL_ALWAYS_SOFTWARE":"1", "WEBKIT_DISABLE_DMABUF_RENDERER":"1"}
binary_dir = pathlib.Path(os.environ.get("OINK_TEST_BIN_DIR", str(root / "target/debug")))
binary = binary_dir / "oink"
cli = binary_dir / "oink-agent"
log = open(directory/"process.log","wb")
app = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=log)
summary = {"uiStarted":True, "ipcReady":False, "nativeWindowVisible":False, "gracefulStop":False, "renderedPixels":False}
try:
    deadline = time.monotonic()+30
    while time.monotonic()<deadline:
        if app.poll() is not None: break
        status = subprocess.run([str(cli),"status"],env=env,capture_output=True,text=True,timeout=4)
        if status.returncode == 0:
            value = json.loads(status.stdout)
            summary["ipcReady"] = isinstance(value.get("status"),dict)
        native = subprocess.run(["xwininfo","-root","-tree"],env=env,capture_output=True,text=True,timeout=3)
        # A sized, mapped product WebView proves more than an alive GTK event loop.
        candidates = [line for line in native.stdout.splitlines() if '"Oink Ubuntu (modified)"' in line]
        for line in candidates:
            xid = line.split()[0]
            info = subprocess.run(["xwininfo","-id",xid],env=env,capture_output=True,text=True,timeout=3).stdout
            if "Map State: IsViewable" in info: summary["nativeWindowVisible"]=True
        if summary["ipcReady"] and summary["nativeWindowVisible"]: break
        time.sleep(.2)
    if summary["nativeWindowVisible"]:
        import gi
        gi.require_version("Gdk","3.0")
        from gi.repository import Gdk
        gi.require_version("GdkX11","3.0")
        from gi.repository import GdkX11
        # Map/DOM-ready precedes the first composited WebKit frame. Verify pixels
        # on the product window itself rather than only its X11 mapping state.
        screen = GdkX11.X11Window.foreign_new_for_display(Gdk.Display.get_default(), int(xid,16))
        # Welcome elements enter through staggered animations ending after 2.4s.
        time.sleep(3)
        for attempt in range(80):
            time.sleep(.2)
            pixbuf = Gdk.pixbuf_get_from_window(screen,0,0,screen.get_width(),screen.get_height())
            if pixbuf is not None:
                data = pixbuf.get_pixels(); stride = pixbuf.get_rowstride(); channels = pixbuf.get_n_channels()
                w = pixbuf.get_width(); h = pixbuf.get_height()
                content = b"".join(data[y*stride+100*channels:y*stride+(w-100)*channels] for y in range(100,h-60,5))
            if pixbuf is not None and len(set(content)) > 32:
                summary["renderedPixels"]=True
                pixbuf.savev(str(directory/"preview.png"),"png",[],[])
                break
    stopped = subprocess.run([str(cli),"stop"],env=env,capture_output=True,text=True,timeout=15)
    try: app.wait(timeout=5)
    except subprocess.TimeoutExpired: pass
    summary["gracefulStop"] = stopped.returncode == 0 and app.poll() is not None
finally:
    # Never match another installation: both executable and private OINK_HOME
    # must identify this test before a process receives a cleanup signal.
    for process in pathlib.Path("/proc").iterdir():
        if not process.name.isdigit(): continue
        try:
            if (process/"status").stat().st_uid != os.getuid(): continue
            executable = (process/"exe").resolve()
            if executable not in [binary.resolve(),(binary_dir/"oink-backend").resolve()]: continue
            values = (process/"environ").read_bytes().split(b"\0")
            if ("OINK_HOME="+str(directory)).encode() in values: os.kill(int(process.name),signal.SIGTERM)
        except (OSError,ValueError): pass
    log.close()
summary["artifacts"] = str(directory)
(directory/"verification.json").write_text(json.dumps(summary,indent=2))
print(json.dumps(summary))
if not all(summary[k] for k in ["ipcReady","nativeWindowVisible","renderedPixels","gracefulStop"]): raise SystemExit(1)
