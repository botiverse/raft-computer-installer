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
from real import assert_stopped, HANDS

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
        manifest = {'version': version, 'targets': {}}
        selections = {}
        for target in TARGETS:
            platform, arch = target.split('-')
            query = urllib.parse.urlencode(dict(product_type='cli-binary', current_version='0.0.0', version=version, platform=platform, arch=arch))
            _, selection = fetch_json(HANDS + '/public/v2/apps/raft-computer-cli/updates/check?' + query)
            if not selection.get('update_available') or selection.get('release', {}).get('version') != version:
                raise ValueError('version identity changed: ' + version)
            artifact = selection['artifact']
            release_id = selection['release']['id']
            prefix = HANDS + '/dl/raft-computer-cli/releases/' + release_id + '/' + target
            if artifact.get('platform') != platform or artifact.get('arch') != arch or artifact.get('download_url') != prefix:
                raise ValueError('platform or hosted URL identity changed: ' + target)
            def entry(identity, filename, expected_url):
                if identity.get('download_url') != expected_url:
                    raise ValueError('representation does not belong to fixed Hands release')
                return {'file': filename, 'sha256': identity['sha256'], 'size': identity['size_bytes'], 'download_url': expected_url}
            raw = entry(artifact, 'raft-computer-' + target, prefix)
            if artifact.get('gzip'):
                raw['gz'] = entry(artifact['gzip'], raw['file'] + '.gz', prefix + '.gz')
            wasm = entry(artifact['photon_wasm'], 'photon_rs_bg.wasm', prefix + '?kind=photon-wasm')
            if version in pinned:
                for actual, expected in ((raw, pinned[version]['targets'][target]), (wasm, pinned[version]['photonWasm'])):
                    if any(actual[k] != expected[k] for k in ('sha256', 'size')):
                        raise ValueError('fixed baseline artifact identity changed: ' + version)
            if version == latest:
                matches = [a for a in authority['assets'] if a['platform'] == platform and a['arch'] == arch and a['filetype'] == 'binary' and a['variant'] is None]
                if len(matches) != 1 or matches[0]['sha256'] != raw['sha256'] or matches[0]['size_bytes'] != raw['size']:
                    raise ValueError('main authority and artifact disagree: ' + target)
            manifest['targets'][target] = raw
            if 'photonWasm' in manifest and manifest['photonWasm']['sha256'] != wasm['sha256']:
                raise ValueError('WASM differs across targets')
            manifest['photonWasm'] = wasm
            selections[target] = selection
        releases[version] = {'manifest': manifest, 'selections': selections}
    return {'schema': 'installer-cold-matrix/v2', 'createdAt': datetime.now(timezone.utc).isoformat(),
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
                wasm = release['selections'][TARGET]['artifact']['photon_wasm']
                assets = [manifest['targets'][TARGET], {'file': 'photon_rs_bg.wasm', 'sha256': wasm['sha256'], 'size': wasm['size_bytes'], 'download_url': wasm['download_url']}]
                if manifest['targets'][TARGET].get('gz'):
                    assets.append(manifest['targets'][TARGET]['gz'])
                for asset in assets:
                    filename = asset['file']
                    if Path(filename).name != filename or '/' in filename or '\\' in filename:
                        raise ValueError('asset filename is not a basename')
                    path = self.root / (version + '-' + filename)
                    checked_download(asset['download_url'], path, asset)
                    parts = urllib.parse.urlsplit(asset['download_url'])
                    self.product_files[parts.path + ('?' + parts.query if parts.query else '')] = path
        except BaseException:
            self.close()
            raise

    def respond(self, handler):
        url = urllib.parse.urlsplit(handler.path)
        path = urllib.parse.unquote(url.path)
        parts = path.strip('/').split('/')
        key = path + ('?' + url.query if url.query else '')
        if key in self.product_files:
            file = self.product_files[key]
            self.requests.append(handler.path)
            handler.send_response(200)
            handler.send_header('Content-Length', str(file.stat().st_size))
            handler.end_headers()
            with file.open('rb') as data:
                shutil.copyfileobj(data, handler.wfile)
            return
        body = None
        if path == '/public/v2/apps/raft-computer-cli/updates/check':
            query = urllib.parse.parse_qs(url.query)
            version = query.get('version', [self.plan['latest']])[0]
            if version in self.plan['releases']:
                body = json.loads(json.dumps(self.plan['releases'][version]['selections'][TARGET]))
                artifact = body['artifact']
                for identity in (artifact, artifact.get('gzip'), artifact['photon_wasm']):
                    if identity:
                        remote = urllib.parse.urlsplit(identity['download_url'])
                        identity['download_url'] = self.base + remote.path + ('?' + remote.query if remote.query else '')
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
    if plan['schema'] != 'installer-cold-matrix/v2':
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
