"""No user-facing wording may name the native installer.

The Rust installer is an internal implementation detail: users only see the
entry scripts and the Computer commands. Every string literal in the Rust
sources and every user-facing line in the entry scripts is scanned for the
word "installer". The allowlist is explicit and exact: the binary's own name
(paths, the download key, the protocol tag), its environment-variable prefix,
the scratch-probe prefix, the state directory segment and the two on-disk
file-name literals the supervisor uses; in the entry scripts, environment
variable names (RAFT_COMPUTER_INSTALLER_*) are names, not wording. Test
modules are excluded (they assert on the word).
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ALLOWED_SUBSTRINGS = (
    "raft-computer-installer",      # the binary's own name: paths, dl key, protocol tag
    "RAFT_COMPUTER_INSTALLER",      # environment variable prefix
    "raft-installer-probe-",        # scratch directory prefix, never printed
)
ALLOWED_EXACT = {
    '"installer"', '"installer.exe"',   # supervisor's on-disk file names
    '"computer/installer"',             # state directory segment (on-disk layout, not wording)
}
ENV_NAME = re.compile(r"\b[A-Z_]*INSTALLER_[A-Z_]+\b")  # env-var names such as RAFT_COMPUTER_INSTALLER_CHANNEL
STRING = re.compile(r'"(?:[^"\\]|\\.)*"')


def rust_offenders():
    found = []
    for path in sorted((ROOT / "rust").glob("*.rs")):
        in_tests = False
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if "#[cfg(test)]" in line:
                in_tests = True
            if in_tests or line.lstrip().startswith("//"):
                continue
            for literal in STRING.findall(line):
                if "installer" not in literal.lower():
                    continue
                if literal in ALLOWED_EXACT or any(a in literal for a in ALLOWED_SUBSTRINGS):
                    continue
                found.append(f"{path.name}:{number}: {literal}")
    return found


def script_offenders():
    found = []
    for name, pattern in (("install.sh", re.compile(r'err "([^"]*)"')),
                          ("install.ps1", re.compile(r"Fail '([^']*)'|Fail \"([^\"]*)\"|WriteLine\('([^']*)'"))):
        text = (ROOT / "bootstrap" / name).read_text(encoding="utf-8")
        for number, line in enumerate(text.splitlines(), 1):
            for match in pattern.finditer(line):
                message = next(g for g in match.groups() if g is not None)
                if "installer" in ENV_NAME.sub("", message).lower():
                    found.append(f"{name}:{number}: {message}")
    return found


class NoInstallerWording(unittest.TestCase):
    def test_rust_string_literals_never_name_the_installer(self):
        self.assertEqual(rust_offenders(), [])

    def test_entry_script_user_lines_never_name_the_installer(self):
        self.assertEqual(script_offenders(), [])

    def test_scanner_sees_a_planted_offender(self):
        # The scanner must be able to fail: a synthetic line with the word
        # outside the allowlist is reported.
        line = '            return Err(invalid("installer receipt too large"));'
        literals = [l for l in STRING.findall(line) if "installer" in l.lower()]
        self.assertEqual(literals, ['"installer receipt too large"'])
        self.assertFalse(any(a in literals[0] for a in ALLOWED_SUBSTRINGS) or literals[0] in ALLOWED_EXACT)
