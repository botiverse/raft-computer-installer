import json
from pathlib import Path
import tempfile
import unittest
import urllib.parse
from unittest.mock import patch

from matrix import cases_for, make_plan, version_key, checked_download


class MatrixContract(unittest.TestCase):
    def test_fixed_pair_survives_moving_latest(self):
        for latest in ('1.0.32', '1.0.39'):
            cases = cases_for('full', latest)
            self.assertIn(dict(kind='upgrade', source='1.0.17', target='1.0.32', selection='exact'), cases)
            self.assertIn(dict(kind='upgrade', source='1.0.17', target=latest, selection='main'), cases)
            self.assertEqual(len(cases), len({json.dumps(c, sort_keys=True) for c in cases}))
            self.assertFalse(any(c['kind'] == 'upgrade' and c['source'] == c['target'] for c in cases))

    def test_candidate_is_real_semver_and_covers_both_fixed_sources(self):
        cases = cases_for('candidate', candidate='1.0.33-rc.1')
        self.assertEqual({c['source'] for c in cases if c['kind'] == 'upgrade'}, {'1.0.17', '1.0.32'})
        self.assertLess(version_key('1.0.33-rc.2'), version_key('1.0.33-rc.10'))
        self.assertLess(version_key('1.0.33-rc.10'), version_key('1.0.33'))
        for invalid in ('../1.0.33', '1.0.33-rc.01', 'latest'):
            with self.assertRaises(ValueError):
                version_key(invalid)
        with self.assertRaises(ValueError):
            cases_for('candidate')
        with self.assertRaises(ValueError):
            cases_for('candidate', candidate='1.0.16')

    def test_manifest_change_and_missing_platform_fail_closed(self):
        # Real baseline metadata, but altered remote content must not redefine it.
        altered = {'version': '1.0.17', 'targets': {}}
        with patch('matrix.fetch_json', return_value=(b'changed', altered)):
            with self.assertRaisesRegex(ValueError, 'identity changed'):
                make_plan('control')

    def test_download_checks_bytes_not_http_success(self):
        import io
        import hashlib
        with tempfile.TemporaryDirectory() as root:
            output = Path(root) / 'binary'
            expected = {'size': 4, 'sha256': hashlib.sha256(b'good').hexdigest()}
            with patch('matrix.urllib.request.urlopen', return_value=io.BytesIO(b'evil')):
                with self.assertRaisesRegex(ValueError, 'frozen identity'):
                    checked_download('https://example.invalid', output, expected)
            with patch('matrix.urllib.request.urlopen', return_value=io.BytesIO(b'goodextra')):
                with self.assertRaisesRegex(ValueError, 'frozen size'):
                    checked_download('https://example.invalid', output, expected)

    def test_main_is_resolved_once_and_cross_checked_for_every_platform(self):
        import hashlib
        from matrix import TARGETS
        manifests = {}
        for version in ('1.0.17', '1.0.32'):
            manifests[version] = {'version': version, 'targets': {t: {'file': 'binary', 'size': 4, 'sha256': 'a' * 64} for t in TARGETS}}
        assets = [{'platform': t.split('-')[0], 'arch': t.split('-')[1], 'variant': None,
                   'filetype': 'binary', 'size_bytes': 4, 'sha256': 'a' * 64, 'download_url': 'must-not-persist'} for t in TARGETS]
        latest = {'build': {'id': 'release-build', 'version': '1.0.32'}, 'assets': assets}
        def fetch(url):
            import urllib.parse
            if '/latest?' in url:
                data = latest
            else:
                query = urllib.parse.parse_qs(urllib.parse.urlsplit(url).query)
                v = query['version'][0]
                target = query['platform'][0] + '-' + query['arch'][0]
                base = 'https://hands.build/dl/raft-computer-cli/releases/r-' + v.replace('.', '-') + '/' + target
                data = {'update_available': True, 'release': {'id': 'r-' + v.replace('.', '-'), 'version': v},
                        'artifact': {'platform': query['platform'][0], 'arch': query['arch'][0], 'sha256': 'a' * 64,
                                     'size_bytes': 4, 'download_url': base,
                                     'photon_wasm': {'sha256': 'c' * 64, 'size_bytes': 2, 'download_url': base + '?kind=photon-wasm'}}}
            return json.dumps(data).encode(), data
        baseline = {'releases': [{'version': v, 'targets': m['targets'], 'photonWasm': {'sha256': 'c' * 64, 'size': 2}} for v, m in manifests.items()]}
        with tempfile.TemporaryDirectory() as root:
            file = Path(root) / 'baselines.json'
            file.write_text(json.dumps(baseline))
            with patch('matrix.BASELINES', file), patch('matrix.fetch_json', side_effect=fetch) as get:
                plan = make_plan('full')
                self.assertEqual(sum('/latest?' in c.args[0] for c in get.call_args_list), 1)
                update_queries = [urllib.parse.parse_qs(urllib.parse.urlsplit(c.args[0]).query)
                                  for c in get.call_args_list if '/updates/check?' in c.args[0]]
                self.assertTrue(update_queries)
                self.assertTrue(all(query.get('current_version_code') == ['0'] for query in update_queries))
                self.assertNotIn('must-not-persist', json.dumps(plan))
                latest['assets'][-1]['sha256'] = 'b' * 64
                with self.assertRaisesRegex(ValueError, 'authority and artifact disagree'):
                    make_plan('full')


class CleanupRule(unittest.TestCase):
    """matrix.clean_up: leftover case processes FAIL; outside file locks warn."""

    @classmethod
    def setUpClass(cls):
        from harness import ReleaseServer
        cls.server = ReleaseServer()
        cls.server.publish('1.0.0')

    @classmethod
    def tearDownClass(cls):
        cls.server.close()

    def machine(self):
        machine = self.server.machine()
        self.server.machines.remove(machine)
        return machine

    def test_process_outliving_a_claimed_stop_fails_and_is_terminated(self):
        from harness import processes_under
        from matrix import clean_up
        machine = self.machine()
        machine.preinstall('1.0.0', running=True)
        self.assertTrue(processes_under(machine.home), 'positive control: the running service is visible')
        # A product that claims it stopped while its service still runs.
        fields, failed = clean_up(machine, lambda: None)
        self.assertTrue(failed)
        self.assertTrue(fields['leftoverProcesses'])
        self.assertIn('outlived', fields['cleanupError'])
        self.assertEqual(processes_under(machine.home), [])
        self.assertTrue(machine.home.exists(), 'evidence is retained')
        import shutil
        shutil.rmtree(machine.home)

    def test_stopped_case_is_removed(self):
        from matrix import clean_up
        machine = self.machine()
        machine.preinstall('1.0.0')
        fields, failed = clean_up(machine, lambda: None)
        self.assertEqual((fields, failed), ({'cleanup': 'removed'}, False))
        self.assertFalse(machine.home.exists())

    def test_transient_busy_file_is_retried(self):
        import shutil
        from matrix import clean_up
        machine = self.machine()
        machine.preinstall('1.0.0')
        real, calls = shutil.rmtree, []
        def busy_twice(*args, **kwargs):
            calls.append(1)
            if len(calls) <= 2:
                raise PermissionError(13, 'The process cannot access the file because it is being used by another process')
            return real(*args, **kwargs)
        with patch('harness.shutil.rmtree', side_effect=busy_twice):
            fields, failed = clean_up(machine, lambda: None)
        self.assertEqual((fields, failed), ({'cleanup': 'removed', 'cleanupRetries': 2}, False))

    def test_lock_without_case_process_warns_without_failing(self):
        import shutil
        from matrix import clean_up
        machine = self.machine()
        machine.preinstall('1.0.0')
        always_busy = PermissionError(13, 'busy')
        with patch('harness.shutil.rmtree', side_effect=always_busy), patch('harness.time.monotonic', side_effect=[0, 0, 100]):
            fields, failed = clean_up(machine, lambda: None)
        self.assertFalse(failed)
        self.assertIn('cleanupWarning', fields)
        self.assertTrue(fields['cleanup'].startswith('retained'))
        shutil.rmtree(machine.home)

    def test_unreadable_process_table_or_unproven_stop_fails(self):
        import shutil
        from matrix import clean_up
        for broken in ('table', 'stop'):
            machine = self.machine()
            machine.preinstall('1.0.0')
            def stop():
                if broken == 'stop':
                    raise AssertionError('product status failed; stopped state is unproven')
            with patch('matrix.processes_under', side_effect=OSError('ps unavailable')):
                fields, failed = clean_up(machine, stop)
            self.assertTrue(failed, broken)
            self.assertIn('cleanupError', fields)
            shutil.rmtree(machine.home)

    def test_process_table_sees_this_process(self):
        import os
        from harness import process_table
        self.assertIn(os.getpid(), {pid for pid, _ in process_table()})
