"""Interactive Windows verification for SlimStore. Python is a development tool only."""
import ctypes as c
from ctypes import wintypes as w
import base64
import pathlib
import subprocess
import sys
import time

user = c.windll.user32
user.SetProcessDpiAwarenessContext(c.c_void_p(-4))
user.MonitorFromWindow.restype = w.HANDLE
user.WindowFromPoint.restype = w.HWND

class Monitor(c.Structure):
    _fields_ = [("size", w.DWORD), ("monitor", w.RECT), ("work", w.RECT), ("flags", w.DWORD)]

def rect(hwnd, client=False):
    bounds = w.RECT()
    assert (user.GetClientRect if client else user.GetWindowRect)(hwnd, c.byref(bounds))
    if client:
        origin = w.POINT()
        assert user.ClientToScreen(hwnd, c.byref(origin))
        return origin.x, origin.y, origin.x + bounds.right, origin.y + bounds.bottom
    return bounds.left, bounds.top, bounds.right, bounds.bottom

def work_area(hwnd):
    info = Monitor()
    info.size = c.sizeof(info)
    assert user.GetMonitorInfoW(user.MonitorFromWindow(hwnd, 2), c.byref(info))
    return info.work.left, info.work.top, info.work.right, info.work.bottom

def wait_until(predicate, about, seconds=5):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if result := predicate():
            return result
        time.sleep(.05)
    raise AssertionError(about)

def window(pid):
    result = []
    @c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
    def collect(hwnd, _):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        if owner.value == pid and user.IsWindowVisible(hwnd) and user.GetWindowTextLengthW(hwnd):
            result.append(hwnd)
        return True
    user.EnumWindows(collect, 0)
    return next(iter(result), None)

def focus(hwnd):
    user.SetForegroundWindow(hwnd)
    wait_until(lambda: user.GetForegroundWindow() == hwnd, "SlimStore could not take focus")

def mouse(x, y, mode=0):
    assert user.SetCursorPos(x, y)
    time.sleep(.05)
    if mode & 1:
        user.mouse_event(2, 0, 0, 0, 0)
    if mode & 2:
        user.mouse_event(4, 0, 0, 0, 0)

def click(point):
    mouse(*point, 1)
    time.sleep(.06)
    mouse(*point, 2)
    time.sleep(.18)

def drag(start, end):
    mouse(*start, 1)
    for step in range(1, 13):
        mouse(*(a + (b-a)*step//12 for a,b in zip(start,end)))
    mouse(*end, 2)
    time.sleep(.25)

def keys(*codes):
    for key in codes:
        user.keybd_event(key, 0, int(key in (0x5b, 0x25, 0x26, 0x28)), 0)
        time.sleep(.06)
    for key in reversed(codes):
        user.keybd_event(key, 0, 2 | int(key in (0x5b, 0x25, 0x26, 0x28)), 0)
        time.sleep(.06)
    time.sleep(.25)

def capture(pid, out, name):
    subprocess.run(["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass",
                    "-File", "scripts/capture.ps1", "-ProcessId", str(pid),
                    "-Destination", str(out / (name + ".png")), "-ClientOnly"], check=True)

def taskbar_button():
    command = """
    Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
    $root = [Windows.Automation.AutomationElement]::RootElement
    $condition = New-Object Windows.Automation.PropertyCondition (
        [Windows.Automation.AutomationElement]::ClassNameProperty, 'Shell_TrayWnd'
    )
    $taskbar = $root.FindFirst([Windows.Automation.TreeScope]::Children, $condition)
    if ($taskbar) {
        foreach($item in $taskbar.FindAll([Windows.Automation.TreeScope]::Descendants,
                                          [Windows.Automation.Condition]::TrueCondition)) {
            if($item.Current.Name -like 'SlimStore*' -and
               $item.Current.ControlType -eq [Windows.Automation.ControlType]::Button) {
                $rect = $item.Current.BoundingRectangle
                if(!$rect.IsEmpty -and !$item.Current.IsOffscreen) {
                    Write-Output ([string][int]($rect.X + $rect.Width/2) + ',' +
                                  [string][int]($rect.Y + $rect.Height/2))
                    break
                }
            }
        }
    }
    """
    result = subprocess.run(["powershell.exe", "-NoProfile", "-EncodedCommand",
                             base64.b64encode(command.encode("utf-16le")).decode()],
                            capture_output=True, text=True, timeout=10)
    if result.returncode == 0 and "," in result.stdout:
        return tuple(map(int, result.stdout.strip().split(",")))
    return None

def exercise(exe, out, baseline=False):
    app = subprocess.Popen([str(exe)])
    def log(text):
        print(text, flush=True)
    try:
        hwnd = wait_until(lambda: window(app.pid), "No SlimStore window visible")
        focus(hwnd)
        time.sleep(.4)
        scale = user.GetDpiForWindow(hwnd) / 96
        px = lambda x: round(x * scale)
        def point(x, y):
            l,t,r,b = rect(hwnd, True)
            return (l + px(x) if x >= 0 else r + px(x), t + px(y) if y >= 0 else b + px(y))
        if baseline:
            click(point(-438, 16))
            capture(app.pid, out, "home-light")
            normal = rect(hwnd, True)
            click(point(-60, 26))
            log(f"Baseline maximize click IsZoomed={user.IsZoomed(hwnd)}, before={normal}, after={rect(hwnd, True)}")
            return
        wl, wt, wr, wb = work_area(hwnd)
        # Center a known window size so pointer gestures can be observed on the desktop.
        assert user.MoveWindow(hwnd, wl+100,wt+70,px(1024),px(768), True)
        time.sleep(.3)
        normal = rect(hwnd, True)
        def check_max():
            wait_until(lambda: user.IsZoomed(hwnd), "maximize did not set IsZoomed")
            assert rect(hwnd, True) == work_area(hwnd), (rect(hwnd, True), work_area(hwnd))
        def check_restore(previous):
            wait_until(lambda: not user.IsZoomed(hwnd), "restore did not clear IsZoomed")
            assert rect(hwnd, True) == previous, (rect(hwnd, True), previous)
        keys(0x5b,0x26)
        check_max()
        keys(0x5b,0x28)
        check_restore(normal)
        keys(0x5b,0x25)
        half_work = (wl,wt,(wl+wr)//2,wb)
        wait_until(lambda: rect(hwnd, True) == half_work, "Win+Left did not finish snapping")
        snapped = rect(hwnd, True)
        log(f"PASS Win+Up, Win+Down and Win+Left, snapped client {snapped}")
        keys(0x1b)  # Dismiss the shell's Snap Assist picker.
        focus(hwnd)
        assert user.MoveWindow(hwnd, wl+100,wt+70,px(1024),px(768), True)
        time.sleep(.3)
        normal = rect(hwnd, True)
        for _ in range(2):
            focus(hwnd)
            click(point(-66, 27))
            check_max()
            capture(app.pid, out, "maximized")
            focus(hwnd)
            click(point(-66, 27))
            check_restore(normal)
        log(f"PASS two real caption maximize/restore cycles, client work area {work_area(hwnd)} and restored {normal}")
        mouse(*point(-22, 27))
        time.sleep(.12)
        capture(app.pid, out, "caption-close-hover")
        def double_click_title():
            region = point(-150, 27)
            packed = (region[1] << 16) | (region[0] & 0xffff)
            assert user.SendMessageW(hwnd, 0x84, 0, packed) == 2
            for _ in range(2):
                mouse(*region, 1)
                time.sleep(.08)
                mouse(*region, 2)
                time.sleep(.08)

        double_click_title()
        check_max()
        double_click_title()
        check_restore(normal)
        double_click_title()
        check_max()
        start = point(-150, 27)
        drag(start, (start[0]-240, start[1]+140))
        assert not user.IsZoomed(hwnd), "drag from maximized did not restore"
        log("PASS title double-click maximize and restore, plus drag from maximized")
        start = point(-150, 27)
        before = rect(hwnd, True)
        drag(start, (start[0]-44, start[1]+22))
        after = rect(hwnd, True)
        assert after[0] < before[0] and after[1] > before[1] and after[2]-after[0] == before[2]-before[0]
        log("PASS normal title drag")
        for x, y in ((34, 30), (120, 30), (110, 470)):
            origin = point(x, y)
            packed = (origin[1] << 16) | (origin[0] & 0xffff)
            assert user.SendMessageW(hwnd, 0x84, 0, packed) == 2
            before = rect(hwnd, True)
            drag(origin, (origin[0] + 30, origin[1] + 20))
            after = rect(hwnd, True)
            assert after == (before[0]+30, before[1]+20, before[2]+30, before[3]+20), (before, after)
        for y in (94, 419):
            nav = point(110, y)
            packed = (nav[1] << 16) | (nav[0] & 0xffff)
            assert user.SendMessageW(hwnd, 0x84, 0, packed) == 1
        log("PASS icon, title, and empty sidebar drag while navigation stays clickable")
        user.MoveWindow(hwnd, wl+100,wt+70,px(1024),px(720),True)
        time.sleep(.25)
        before = rect(hwnd, True)
        start = point(-2, 200)
        drag(start, (start[0]+55, start[1]))
        after = rect(hwnd, True)
        assert after[2] > before[2] and after[3] == before[3]
        start = point(-2,-2)
        drag(start, (start[0]+55,start[1]+40))
        corner = rect(hwnd, True)
        assert corner[2] > after[2] and corner[3] > after[3]
        log("PASS right edge and bottom-right corner resizing")
        before = rect(hwnd, True)
        click(point(-110,27))
        wait_until(lambda: user.IsIconic(hwnd), "minimize did not set IsIconic")
        if button := taskbar_button():
            click(button)
            if not user.IsIconic(hwnd):
                log("PASS taskbar button restored SlimStore")
            else:
                log("UNVERIFIED taskbar click did not restore the window")
        else:
            log("UNVERIFIED taskbar button unavailable to UI Automation")
        if user.IsIconic(hwnd):
            user.ShowWindow(hwnd, 9)
        wait_until(lambda: not user.IsIconic(hwnd), "shell restore did not clear IsIconic")
        assert rect(hwnd, True) == before
        log("PASS caption minimize and shell restore geometry")
        focus(hwnd)
        keys(0x12,0x20)
        menu = c.create_string_buffer(72)
        c.cast(menu,c.POINTER(c.c_uint32))[0]=72 if c.sizeof(c.c_void_p)==8 else 48
        assert user.GetGUIThreadInfo(0, menu), "GetGUIThreadInfo failed"
        flags = c.cast(menu,c.POINTER(c.c_uint32))[1]
        keys(0x1b)
        assert flags & 0xc, f"Alt+Space did not enter a system menu, flags={flags}"
        log("PASS Alt+Space system menu and Escape")
        user.MoveWindow(hwnd, wl+100,wt+70,px(1024),px(720),True)
        time.sleep(.25)
        start = point(-150,27)
        drag(start,(wl,(wt+wb)//2))
        edge_snap = rect(hwnd,True)
        if abs(edge_snap[0]-wl) <= px(8) and edge_snap[3]-edge_snap[1] >= wb-wt-px(16):
            log(f"PASS Aero Snap by title drag {edge_snap}")
        else:
            log(f"UNVERIFIED Aero Snap by title drag {edge_snap}")
        for width,height in [(820,600),(1024,768),(1280,800),(1600,900)]:
            user.MoveWindow(hwnd,wl+16,wt+12,px(width),px(height),True)
            time.sleep(.4)
            l,t,r,b=rect(hwnd,True)
            assert (r-l,b-t) == (px(width),px(height)), (r-l,b-t,width,height)
            capture(app.pid,out,f"geometry-{width}x{height}")
        log("PASS exact client sizes 820x600, 1024x768, 1280x800, 1600x900")
        focus(hwnd)
        keys(0x12,0x73)
        try:
            app.wait(timeout=3)
            log("PASS Alt+F4")
        except subprocess.TimeoutExpired:
            log("UNVERIFIED Alt+F4 key injection did not close the window")
            user.PostMessageW(hwnd, 0x10, 0, 0)
            app.wait(timeout=10)
        assert app.returncode == 0
        reopened = subprocess.Popen([str(exe)])
        try:
            other = wait_until(lambda: window(reopened.pid), "reopen failed")
            focus(other)
            time.sleep(.3)
            l,t,r,b=rect(other,True)
            click((r-px(22),t+px(27)))
            reopened.wait(timeout=10)
            assert reopened.returncode == 0
        finally:
            if reopened.poll() is None:
                reopened.terminate()
        log("PASS clean exit, reopen and Close caption")
    finally:
        if app.poll() is None:
            if hwnd := window(app.pid):
                user.PostMessageW(hwnd, 0x10, 0, 0)
            try:
                app.wait(timeout=5)
            except subprocess.TimeoutExpired:
                app.terminate()

if __name__ == "__main__":
    output = pathlib.Path(sys.argv[2]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    exercise(pathlib.Path(sys.argv[1]).resolve(), output, "--baseline" in sys.argv)
