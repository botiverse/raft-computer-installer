"""Source-level contracts of the entry scripts that no CI runner can exercise.

The Windows entry script must not read the machine architecture through
System.Runtime.InteropServices.RuntimeInformation: an interactive Windows
PowerShell 5.1 console has PSReadLine loaded, and PSReadLine carries a
same-named stub type without OSArchitecture that shadows the real one, so the
type literal yields $null and the script dies before its first network request
(task #877, issue #15). CI sessions are non-interactive and the shadowing could
not be reproduced there (measured: PSReadLine import, a type accelerator and an
exact-name Add-Type stub all left the real type in place), so the contract is
enforced on the source and proved able to fail with a planted offender.

The Windows entry script is also pasted as `irm .../install.ps1 | iex` or run
as `& ([scriptblock]::Create((irm ...)))`. There it has no script file, and
`exit` ends the user's PowerShell session: the window closes and an error
message vanishes with it. Only a run from a file may exit; otherwise the
script reports its code through $LASTEXITCODE. No CI runner here pastes into
an interactive console, so this too is a source contract.
"""
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FORBIDDEN = re.compile(r"RuntimeInformation", re.IGNORECASE)
EXIT = re.compile(r"(?<![\w$-])exit(?![\w-])", re.IGNORECASE)
GUARDED_EXIT = "if ($runAsFile) { exit $code }"
# A .NET exception message may name the URL it failed on, credentials and
# signed query included; the Windows script prints it only through Redacted.
EXCEPTION_MESSAGE = re.compile(r"(?<!Redacted )\$_\.Exception\.Message")


def production_lines(text):
    for number, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        yield number, line


def runtime_information_offenders(text):
    return [f"{number}: {line.strip()}" for number, line in production_lines(text) if FORBIDDEN.search(line)]


def unredacted_messages(text):
    return [f"{number}: {line.strip()}" for number, line in production_lines(text) if EXCEPTION_MESSAGE.search(line)]


def unguarded_exits(text):
    return [f"{number}: {line.strip()}" for number, line in production_lines(text)
            if EXIT.search(line) and line.strip() != GUARDED_EXIT]


class EntryScriptContract(unittest.TestCase):
    def test_windows_entry_script_never_references_runtime_information(self):
        text = (ROOT / "bootstrap" / "install.ps1").read_text(encoding="utf-8")
        self.assertEqual(runtime_information_offenders(text), [])

    def test_windows_entry_script_reads_the_architecture_from_the_environment(self):
        text = (ROOT / "bootstrap" / "install.ps1").read_text(encoding="utf-8")
        self.assertIn("$env:PROCESSOR_ARCHITEW6432", text)
        self.assertIn("$env:PROCESSOR_ARCHITECTURE", text)
        self.assertIn("-ne 'AMD64'", text)

    def test_windows_entry_script_enables_progress_for_the_executable_only(self):
        text = (ROOT / "bootstrap" / "install.ps1").read_text(encoding="utf-8")
        self.assertIn("$ProgressPreference = if ($showProgress) { 'Continue' }", text)
        self.assertIn("Download $sumsUrl $sums $false", text)
        self.assertIn("Download $binaryUrl $cli $true", text)

    def test_windows_entry_script_exits_only_when_run_as_a_file(self):
        text = (ROOT / "bootstrap" / "install.ps1").read_text(encoding="utf-8")
        self.assertEqual(unguarded_exits(text), [])
        lines = [line.strip() for _, line in production_lines(text) if line.strip()]
        # The file-ness probe tolerates Set-StrictMode, the guarded exit is
        # the last decision, and a pasted run ends by setting the code.
        self.assertIn("try { $runAsFile = [bool]$PSCommandPath } catch { }", lines)
        self.assertEqual(lines[-2:], [GUARDED_EXIT, "$global:LASTEXITCODE = $code"])

    def test_windows_entry_script_prints_exception_messages_only_redacted(self):
        text = (ROOT / "bootstrap" / "install.ps1").read_text(encoding="utf-8")
        self.assertEqual(unredacted_messages(text), [])
        self.assertGreaterEqual(text.count("Redacted $_.Exception.Message"), 3)

    def test_exception_message_scanner_sees_a_planted_offender(self):
        planted = "  [Console]::Error.WriteLine('failed: ' + $_.Exception.Message)\n"
        self.assertEqual(len(unredacted_messages(planted)), 1)
        self.assertEqual(unredacted_messages("  Fail \"x $(Redacted $_.Exception.Message)\"\n"), [])

    def test_exit_scanner_sees_a_planted_offender(self):
        self.assertEqual(len(unguarded_exits("exit $code\n")), 1)
        self.assertEqual(len(unguarded_exits("  if ($x) { Exit 1 }\n")), 1)
        self.assertEqual(unguarded_exits("# exit 3 means unresolved\n$code = $LASTEXITCODE\n"), [])
        self.assertEqual(unguarded_exits(GUARDED_EXIT + "\n"), [])

    def test_scanner_sees_a_planted_offender(self):
        planted = "  $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()\n"
        self.assertEqual(len(runtime_information_offenders(planted)), 1)
        # A comment that names the type is documentation, not a reference.
        self.assertEqual(runtime_information_offenders("  # never read RuntimeInformation here\n"), [])


if __name__ == "__main__":
    unittest.main()
