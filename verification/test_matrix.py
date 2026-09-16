import json
from pathlib import Path
import tempfile
import unittest
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
            data = latest if '/latest?' in url else manifests[url.split('/')[-2]]
            body = json.dumps(data).encode()
            return body, data
        baseline = {'releases': [{'version': v, 'manifestSHA256': hashlib.sha256(json.dumps(m).encode()).hexdigest()} for v, m in manifests.items()]}
        with tempfile.TemporaryDirectory() as root:
            file = Path(root) / 'baselines.json'
            file.write_text(json.dumps(baseline))
            with patch('matrix.BASELINES', file), patch('matrix.fetch_json', side_effect=fetch) as get:
                plan = make_plan('full')
                self.assertEqual(sum('/latest?' in c.args[0] for c in get.call_args_list), 1)
                self.assertNotIn('must-not-persist', json.dumps(plan))
                latest['assets'][-1]['sha256'] = 'b' * 64
                with self.assertRaisesRegex(ValueError, 'authority and manifest disagree'):
                    make_plan('full')
