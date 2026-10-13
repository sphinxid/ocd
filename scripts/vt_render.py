"""Minimal VT screen renderer: replays a `script` capture and prints the final screen.

`render(raw, cols, rows)` renders a whole capture; `Screen(cols, rows)` renders incrementally (`feed()` new
output as it arrives, `text()` for the current screen), which keeps polling a long-running TUI cheap.
"""
import re
import sys
import unicodedata

CSI = re.compile(r"\x1b\[([?>=]?)([0-9;:]*)([ -/]*)([@-~])")
# A complete escape sequence starting at an ESC (used to hold back a sequence split across feeds).
COMPLETE = re.compile(r"\x1b(\[[?>=]?[0-9;:]*[ -/]*[@-~]|\][^\x07\x1b]*(\x07|\x1b\\)|[^\[\]])")


def width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in "WF" else 1


class Screen:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.screen = [[" "] * cols for _ in range(rows)]
        self.r = self.c = 0
        self.pending = ""

    def feed(self, raw, final=False):
        """Apply `raw`; an escape sequence cut off at the end waits for the next feed unless `final`."""
        raw = self.pending + raw
        self.pending = ""
        if not final:
            esc = raw.rfind("\x1b")
            if esc != -1 and not COMPLETE.match(raw, esc):
                raw, self.pending = raw[:esc], raw[esc:]
        self._apply(raw)
        return self

    def _apply(self, raw):
        cols, rows = self.cols, self.rows
        screen, r, c = self.screen, self.r, self.c
        i = 0
        n = len(raw)
        while i < n:
            ch = raw[i]
            if ch == "\x1b":
                m = CSI.match(raw, i)
                if m:
                    priv, params, _, final = m.groups()
                    nums = [int(p) if p.isdigit() else 0 for p in params.replace(":", ";").split(";")] if params else []
                    a = nums[0] if nums else 0
                    if final == "H" or final == "f":
                        r = max(0, min(rows - 1, (nums[0] if nums and nums[0] else 1) - 1))
                        c = max(0, min(cols - 1, (nums[1] if len(nums) > 1 and nums[1] else 1) - 1))
                    elif final == "A": r = max(0, r - (a or 1))
                    elif final == "B": r = min(rows - 1, r + (a or 1))
                    elif final == "C": c = min(cols - 1, c + (a or 1))
                    elif final == "D": c = max(0, c - (a or 1))
                    elif final == "G": c = max(0, min(cols - 1, (a or 1) - 1))
                    elif final == "d": r = max(0, min(rows - 1, (a or 1) - 1))
                    elif final == "K":
                        if a == 0: screen[r][c:] = [" "] * (cols - c)
                        elif a == 1: screen[r][: c + 1] = [" "] * (c + 1)
                        else: screen[r] = [" "] * cols
                    elif final == "J":
                        if a == 2 or a == 3: screen[:] = [[" "] * cols for _ in range(rows)]
                        elif a == 0:
                            screen[r][c:] = [" "] * (cols - c)
                            for k in range(r + 1, rows): screen[k] = [" "] * cols
                    elif final == "X": screen[r][c:c + (a or 1)] = [" "] * min(a or 1, cols - c)
                    elif final in "hl" and priv == "?" and 1049 in nums:
                        screen[:] = [[" "] * cols for _ in range(rows)]
                    i = m.end()
                    continue
                if raw.startswith("\x1b]", i):
                    end = raw.find("\x07", i)
                    st = raw.find("\x1b\\", i)
                    ends = [e for e in (end, st) if e != -1]
                    i = (min(ends) + (1 if min(ends) == end else 2)) if ends else n
                    continue
                i += 2
                continue
            if ch == "\r": c = 0
            elif ch == "\n":
                if r == rows - 1:  # scroll up (inline UIs such as `ocd mini` print into scrollback)
                    screen.pop(0)
                    screen.append([" "] * cols)
                else:
                    r += 1
            elif ch == "\b": c = max(0, c - 1)
            elif ch >= " ":
                w = width(ch)
                if w and c < cols:
                    screen[r][c] = ch
                    if w == 2 and c + 1 < cols: screen[r][c + 1] = ""
                    c = min(cols - 1, c + w) if c + w < cols else cols - 1
            i += 1
        self.r, self.c = r, c

    def text(self):
        return "\n".join("".join(row).rstrip() for row in self.screen)


def render(raw, cols, rows):
    return Screen(cols, rows).feed(raw, final=True).text()


if __name__ == "__main__":
    data = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
    start = data.find("\n") + 1  # skip "Script started" header
    end = data.rfind("Script done")
    print(render(data[start:end if end > 0 else len(data)], int(sys.argv[2]), int(sys.argv[3])))
