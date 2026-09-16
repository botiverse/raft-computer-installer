#!/usr/bin/env python3
"""Freeze public release identities once, then run independent cold product cases."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import urllib.parse
import urllib.request

from harness import ReleaseServer, ROOT, TARGET, DIST, SUFFIX
from http_client import public_request
from real import assert_stopped, HANDS, CDN

TARGETS = ('darwin-arm64', 'darwin-x64', 'linux-arm64', 'linux-x64', 'win32-x64')
BASELINES = Path(__file__).parent / 'baselines/published-2026-09-16.json'


def version_key(value):
    match = re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?', value)
    if not match:
        raise ValueError('expected exact semver without build metadata')
    pre = match[4]
    identifiers = []
    for part in pre.split('.') if pre else []:
        if part.isdigit() and len(part) > 1 and part[0] == '0':
            raise ValueError('noncanonical numeric prerelease')
        identifiers.append((0, int(part)) if part.isdigit() else (1, part))
    return (*map(int, match.group(1, 2, 3)), (0, tuple(identifiers)) if pre else (1, ()))


def fetch_json(url):
    with urllib.request.urlopen(public_request(url), timeout=60) as response:
        body = response.read(1_048_577)
    if len(body) > 1_048_576:
        raise ValueError('metadata too large')
    return body, json.loads(body)


def cases_for(mode, latest=None, candidate=None):
    L, R = '1.0.17', '1.0.32'
    cases = []
    def add(kind, source, target, selection='exact'):
        if source and version_key(source) > version_key(target) and kind not in ('deny-downgrade', 'allow-downgrade'):
            raise ValueError('upgrade target precedes fixed baseline')
        if kind == 'upgrade' and source == target:
            kind = 'repeat'
        case = dict(kind=kind, source=source, target=target, selection=selection)
        if case not in cases:
            cases.append(case)
    if mode in ('control', 'full'):
        add('upgrade', L, R)
    destinations = ([latest] if latest else []) + ([candidate] if candidate else [])
    for target in dict.fromkeys(destinations):
        add('fresh', None, target)
        add('upgrade', L, target)
        if mode != 'latest':
            add('upgrade', R, target)
        add('repeat', target, target)
        add('adopt-reconstructed', L, target)
        add('repair', target, target)
    if latest:
        # Keep the channel entry even if the byte edge equals a pinned edge.
        add('upgrade', L, latest, 'main')
    if mode == 'full':
        add('deny-downgrade', R, L)
        add('allow-downgrade', R, L)
    if mode == 'candidate' and not candidate:
        raise ValueError('candidate mode requires --candidate')
    return cases


def make_plan(mode, candidate=None):
    if candidate:
        version_key(candidate)
    latest, authority = None, None
    if mode in ('full', 'latest'):
        _, payload = fetch_json(HANDS + '/public/v2/apps/raft-computer-cli/latest?channel=main&product_type=cli-binary')
        latest = payload['build']['version']
        version_key(latest)
        # Do not persist expiring signed URLs; only identity and version data.
        authority = {'build': {'id': payload['build'].get('id'), 'version': latest},
            'assets': [{key: a.get(key) for key in ('platform', 'arch', 'variant', 'filetype', 'size_bytes', 'sha256')}
                       for a in payload['assets']]}
    cases = cases_for(mode, latest, candidate)
    pinned = {r['version']: r for r in json.loads(BASELINES.read_text())['releases']}
    releases = {}
    for version in sorted({c[k] for c in cases for k in ('source', 'target') if c[k]}, key=version_key):
        url = CDN + '/' + version + '/manifest.json'
        body, manifest = fetch_json(url)
        digest = hashlib.sha256(body).hexdigest()
        if version in pinned and digest != pinned[version]['manifestSHA256']:
            raise ValueError('fixed baseline manifest identity changed: ' + version)
        if manifest['version'] != version or any(t not in manifest['targets'] for t in TARGETS):
            raise ValueError('version or platform inventory mismatch: ' + version)
        if version == latest:
            for target in TARGETS:
                platform, arch = target.split('-')
                assets = [a for a in authority['assets'] if a['platform'] == platform and a['arch'] == arch
                          and a['filetype'] == 'binary' and a['variant'] is None]
                expected = manifest['targets'][target]
                if len(assets) != 1 or assets[0]['sha256'] != expected['sha256'] or assets[0]['size_bytes'] != expected['size']:
                    raise ValueError('main authority and manifest disagree: ' + target)
        releases[version] = {'manifestUrl': url, 'manifestSHA256': digest, 'manifest': manifest}
    return {'schema': 'installer-cold-matrix/v1', 'createdAt': datetime.now(timezone.utc).isoformat(),
        'mode': mode, 'installerCommit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        'latest': latest, 'candidate': candidate, 'authority': authority, 'releases': releases, 'cases': cases,
        'excluded': ['authenticated live agents', 'original historical installer layouts', 'unpublished new Computer upgrade API baseline']}


def checked_download(url, destination, expected):
    digest, size = hashlib.sha256(), 0
    with urllib.request.urlopen(public_request(url), timeout=180) as response, destination.open('wb') as out:
        for block in iter(lambda: response.read(1024 * 1024), b''):
            size += len(block)
            if size > expected['size']:
                raise ValueError('download exceeds frozen size')
            digest.update(block)
            out.write(block)
    if size != expected['size'] or digest.hexdigest() != expected['sha256']:
        raise ValueError('download differs from frozen identity')


class FrozenServer(ReleaseServer):
    """Replay frozen public metadata and verified public bytes, never fixture bytes."""
    def __init__(self, plan):
        self.plan = plan
        self.product_files = {}
        super().__init__()
        try:
            for version, release in plan['releases'].items():
                manifest = release['manifest']
                assets = [manifest['targets'][TARGET], manifest['photonWasm']]
                if manifest['targets'][TARGET].get('gz'):
                    assets.append(manifest['targets'][TARGET]['gz'])
                for asset in assets:
                    filename = asset['file']
                    if Path(filename).name != filename or '/' in filename or '\\' in filename:
                        raise ValueError('asset filename is not a basename')
                    path = self.root / (version + '-' + filename)
                    checked_download(CDN + '/' + version + '/' + filename, path, asset)
                    self.product_files['/computer/' + version + '/' + filename] = path
        except BaseException:
            self.close()
            raise

    def respond(self, handler):
        url = urllib.parse.urlsplit(handler.path)
        path = urllib.parse.unquote(url.path)
        parts = path.strip('/').split('/')
        if path in self.product_files:
            file = self.product_files[path]
            self.requests.append(handler.path)
            handler.send_response(200)
            handler.send_header('Content-Length', str(file.stat().st_size))
            handler.end_headers()
            with file.open('rb') as data:
                shutil.copyfileobj(data, handler.wfile)
            return
        body = None
        if len(parts) == 3 and parts[0] == 'computer' and parts[2] == 'manifest.json' and parts[1] in self.plan['releases']:
            body = self.plan['releases'][parts[1]]['manifest']
        elif path == '/public/v2/apps/raft-computer-cli/latest' and urllib.parse.parse_qs(url.query).get('channel') == ['main']:
            body = self.plan['authority']
        if body is not None:
            self.requests.append(handler.path)
            data = json.dumps(body).encode()
            handler.send_response(200)
            handler.send_header('Content-Length', str(len(data)))
            handler.end_headers()
            handler.wfile.write(data)
        else:
            super().respond(handler)


def execute(plan_path, output):
    plan_bytes = plan_path.read_bytes()
    plan = json.loads(plan_bytes)
    if plan['schema'] != 'installer-cold-matrix/v1':
        raise ValueError('unknown matrix schema')
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip()
    if plan['installerCommit'] != head:
        raise ValueError('plan belongs to a different installer commit')
    report = {'planSHA256': hashlib.sha256(plan_bytes).hexdigest(), 'target': TARGET,
        'installerCommit': head, 'installerSHA256': hashlib.sha256((DIST / 'native' / TARGET / ('raft-computer-installer' + SUFFIX)).read_bytes()).hexdigest(),
        'scope': 'published Computer bytes, frozen metadata replay, cold isolated homes',
        'cases': [], 'excluded': plan['excluded']}
    output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        output.write_text(json.dumps(report, indent=2) + '\n')
    save()
    server = None
    failures = []
    try:
        server = FrozenServer(plan)
        for index, case in enumerate(plan['cases']):
            machine = server.machine()
            row = {**case, 'index': index, 'status': 'FAIL', 'receipts': []}
            report['cases'].append(row)
            extra = {'RAFT_COMPUTER_INSTALLER_RELEASE_BASE': server.base + '/installer', 'RAFT_COMPUTER_NO_MODIFY_PATH': '1'}
            def run(args, expected=0):
                result = subprocess.run(machine.command([*args, '--json'], bootstrap=True),
                    env=machine.env(extra), text=True, capture_output=True, timeout=900)
                if result.returncode != expected:
                    raise AssertionError(f'{args}: exit {result.returncode}; {result.stdout[-3000:]} {result.stderr[-1500:]}')
                reply = json.loads(result.stdout)
                assert reply['exitCode'] == expected
                row['receipts'].append(reply)
                return reply
            def cold(version):
                assert machine.self_version() == version
                row['statusEvidence'] = assert_stopped(machine, extra)
                # Prove deployed product and sidecar bytes match the frozen plan.
                manifest = plan['releases'][version]['manifest']
                assert hashlib.sha256(machine.binary.read_bytes()).hexdigest() == manifest['targets'][TARGET]['sha256']
                assert hashlib.sha256((machine.install_dir / 'photon_rs_bg.wasm').read_bytes()).hexdigest() == manifest['photonWasm']['sha256']
            try:
                source, target, kind = case['source'], case['target'], case['kind']
                if source:
                    run(['install', '--version', source])
                    cold(source)
                marker = machine.home / 'matrix-user-data'
                marker.write_text('preserve-' + str(index))
                if kind == 'adopt-reconstructed':
                    shutil.rmtree(machine.k)
                if kind == 'repair':
                    (machine.k / 'operation.json').write_text('synthetic unreadable operation\n')
                args = ['install' if kind == 'fresh' else 'repair' if kind == 'repair' else 'upgrade']
                args += ['--channel', 'main'] if case['selection'] == 'main' else ['--version', target]
                if kind == 'allow-downgrade':
                    args += ['--allow-downgrade']
                before = len(server.requests)
                reply = run(args, 2 if kind == 'deny-downgrade' else 0)
                if case['selection'] == 'main':
                    assert any(p.startswith('/public/v2/apps/raft-computer-cli/latest?') for p in server.requests[before:])
                    assert reply['receipt']['targetVersion'] == plan['latest']
                if kind == 'repeat':
                    assert reply['receipt']['outcome'] == 'up-to-date'
                if kind == 'repair':
                    assert reply['receipt']['outcome'] == 'repaired'
                cold(source if kind == 'deny-downgrade' else target)
                assert marker.read_text() == 'preserve-' + str(index)
                row['status'] = 'PASS'
            except Exception as error:
                row['error'] = str(error)
                failures.append(index)
            finally:
                # Verify stopped before deleting; retain evidence if uncertain.
                try:
                    if machine.binary.exists():
                        subprocess.run([str(machine.binary), 'stop'], env=machine.env(extra), capture_output=True, timeout=60)
                        assert_stopped(machine, extra)
                    machine.close()
                    row['cleanup'] = 'removed'
                except Exception as error:
                    machine.directory._finalizer.detach()
                    row['cleanup'] = 'retained: ' + str(machine.home)
                    row['cleanupError'] = str(error)
                    row['status'] = 'FAIL'
                    failures.append(index)
                server.machines.remove(machine)
                save()
            print(TARGET, index, kind, source, '->', target, row['status'], flush=True)
    except Exception as error:
        report['error'] = str(error)
        failures.append('setup')
    finally:
        if server:
            server.close()
        report['status'] = 'FAIL' if failures else 'PASS'
        save()
    return bool(failures)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    p = sub.add_parser('plan')
    p.add_argument('--mode', choices=('control', 'full', 'latest', 'candidate'), default='full')
    p.add_argument('--candidate')
    p.add_argument('--output', type=Path, required=True)
    r = sub.add_parser('run')
    r.add_argument('--plan', type=Path, required=True)
    r.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.command == 'plan':
        plan = make_plan(args.mode, args.candidate)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(plan, indent=2) + '\n')
        print(json.dumps({'mode': plan['mode'], 'latest': plan['latest'], 'candidate': plan['candidate'], 'cases': len(plan['cases'])}))
        return 0
    return int(execute(args.plan, args.output))


if __name__ == '__main__':
    sys.exit(main())
