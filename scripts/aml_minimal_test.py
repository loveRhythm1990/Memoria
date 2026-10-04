#!/usr/bin/env python3
"""Run synthetic AML Add/Search checks against a local Memoria deployment.

Python 3.11+, standard library only. No Answer/Judge model, gold in requests,
dataset-specific fields, retries, or automatic deletion of existing data.
"""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]


def scenarios():
    result = []
    for i in range(8):
        name = ['Lin', 'Mira', 'Ravi', 'Nora', '林舟', '许青', '顾禾', '陈晓'][i]
        partner = ['Ivo', 'Ada', 'Eli', 'Tara', '周宁', '沈安', '李岚', '何晴'][i]
        city = ['Porto', 'Kyoto', 'Bergen', 'Turin', '杭州', '苏州', '厦门', '成都'][i]
        studio = ['Copper Finch', 'Silver Reed', 'Amber Kite', 'Indigo Moss',
                  '铜雀工坊', '银苇工坊', '琥珀工坊', '青苔工坊'][i]
        en = i < 4
        texts = [
            f'{name} lives in {city}.' if en else f'{name}现在住在{city}。',
            f"{name}'s hiking partner is {partner}." if en else f'{name}的徒步搭档是{partner}。',
            f'{partner} works at {studio}, a ceramics studio.' if en else f'{partner}在陶艺工作室{studio}工作。',
            f'{name}: Yesterday I visited the marine aquarium.' if en else f'{name}：我昨天参观了海洋水族馆。',
            f'{name} prefers short bullet points in written replies.' if en else f'{name}希望书面回复使用简短的要点列表。',
            f'{name}: Do not schedule my meetings before 10 AM.' if en else f'{name}：不要把我的会议安排在上午十点之前。',
            f"{name}'s parcel pickup point is the blue gate." if en else f'{name}的包裹取货点是蓝色大门。',
        ]
        # Deliberate subject and topic changes. These are never expected evidence.
        texts += [
            (f'Other member Visitor-{n} keeps a travel notebook about garden {n}, '
             f'prefers long essays, schedules meetings at 8 AM, and uses the red parcel gate.'
             if en else f'另一位成员访客{n}在花园{n}写旅行笔记，喜欢长篇回复，'
             '会议安排在早上八点，包裹去红色大门领取。') for n in range(32)
        ]
        updated = (f'{name}: My parcel pickup point has changed from the blue gate '
                   'to the green gate. The blue gate is the previous location.' if en else
                   f'{name}：我的包裹取货点已经从蓝色大门改为绿色大门，蓝色大门是以前的地点。')
        confirmation = (f'{name}: Please use the green gate for my next parcel.' if en else
                        f'{name}：下次领取我的包裹请去绿色大门。')
        queries = [
            ('fact', f'Which city does {name} live in?' if en else f'{name}住在哪个城市？', [texts[0]]),
            ('multi_hop', f'Which studio employs {name}\'s hiking partner?' if en else
             f'{name}的徒步搭档在哪家工作室上班？', texts[1:3]),
            ('temporal', f'When did {name} visit the aquarium, relative to the source date?' if en else
             f'结合消息日期，{name}是什么时候去水族馆的？', [texts[3], '2023-05-08T12:00:00']),
            ('preference_rule', f'How should I write replies and schedule meetings for {name}?' if en else
             f'给{name}写回复和安排会议，需要遵守什么偏好与规则？', texts[4:6]),
            ('state_update', f'Where should {name} collect the next parcel, and what was the previous location?' if en else
             f'{name}下次应该在哪里取包裹，以前的取货点又是哪里？', [updated, confirmation]),
        ]
        result.append({'name': name, 'messages': texts, 'update': [updated, confirmation],
                       'queries': queries, 'before_query': f'{name} parcel pickup point' if en else
                       f'{name}的包裹取货点', 'old': texts[6], 'new_marker': updated})
    return result


class Runner:
    def __init__(self, base, key, timeout):
        self.base, self.key, self.timeout = base.rstrip('/'), key, timeout
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.events, self.checks, self.quality = [], [], []

    def check(self, name, ok, details=None):
        self.checks.append({'name': name, 'passed': bool(ok), 'details': details})

    def call(self, kind, payload=None, key='configured'):
        headers = {'Accept': 'application/json'}
        if payload is not None:
            headers['Content-Type'] = 'application/json'
        if key is not None and kind != 'health':
            headers['Authorization'] = 'Bearer ' + (self.key if key == 'configured' else key)
        request = urllib.request.Request(self.base + ('/health' if kind == 'health' else '/aml/' + kind),
                                         data=None if payload is None else json.dumps(payload, ensure_ascii=False).encode(),
                                         headers=headers, method='GET' if kind == 'health' else 'POST')
        started = time.monotonic()
        status, body = None, None
        try:
            with self.http.open(request, timeout=self.timeout) as response:
                status, raw = response.status, response.read()
        except urllib.error.HTTPError as error:
            status, raw = error.code, error.read()
        try:
            body = json.loads(raw)
        except (ValueError, UnicodeDecodeError):
            body = {'non_json_response': True}
        self.events.append({'kind': kind, 'status': status,
                            'elapsed_ms': round((time.monotonic() - started) * 1000, 2)})
        if status == 200 and kind == 'add':
            self.check('add_response_contract', isinstance(body, dict) and body.get('success') is True
                       and all(body.get(k) == payload[k] for k in ('request_id', 'user_id', 'session_id')))
        if status == 200 and kind == 'search':
            items = body.get('data') if isinstance(body, dict) else None
            valid = isinstance(items, list) and len(items) <= payload['top_k']
            if valid:
                valid = all(isinstance(m, dict) and all(isinstance(m.get(k), str) and m[k].strip()
                            for k in ('id', 'content')) and ('score' not in m or
                            type(m['score']) in (int, float) and math.isfinite(m['score'])) for m in items)
                valid = valid and len({m['id'] for m in items}) == len(items)
            self.check('search_response_contract', valid)
        return status, body

    def search(self, user, query, k=100, options=None):
        payload = {'query': query, 'user_id': user, 'top_k': k}
        if options is not None:
            payload['options'] = options
        status, body = self.call('search', payload)
        self.check('search_http_200', status == 200)
        return body.get('data', []) if status == 200 and isinstance(body, dict) else []


def latency(events):
    out = {}
    for kind in ('add', 'search'):
        values = sorted(e['elapsed_ms'] for e in events if e['kind'] == kind and e['status'] == 200)
        if values:
            out[kind] = {'count': len(values), 'mean_ms': round(sum(values)/len(values), 2),
                         'p95_ms': values[math.ceil(.95*len(values))-1]}
    return out


def execute(r, run_id):
    r.check('health', r.call('health')[0] == 200)
    first_request = None
    for i, case in enumerate(scenarios()):
        user = f'eval:{run_id}:synthetic:{i}:用户/opaque'
        for chunk in range(2):
            start = chunk * 20
            messages = [{'role': 'user' if n % 2 == 0 else 'assistant', 'content': text,
                         **({'timestamp': 1683547200000} if start+n == 3 else {})}
                        for n, text in enumerate(case['messages'][start:start+20])]
            request = {'request_id': f'opaque 请求/{chunk}/🧠', 'user_id': user,
                       'session_id': f'session-{chunk}', 'messages': messages}
            status, _ = r.call('add', request)
            r.check('add_http_200', status == 200)
            if first_request is None:
                first_request = request
        # No sleeps/polling: Add success must make source evidence immediately retrievable.
        before = r.search(user, case['before_query'])
        r.check('incremental_before_has_old', any(case['old'] in m['content'] for m in before))
        r.check('incremental_before_no_future', not any(case['new_marker'] in m['content'] for m in before))
        update = {'request_id': 'incremental/update', 'user_id': user, 'session_id': 'later-session',
                  'messages': [{'role': 'user', 'content': text, 'timestamp': 1683633600000}
                               for text in case['update']]}
        r.check('incremental_add_200', r.call('add', update)[0] == 200)
        for category, query, expected in case['queries']:
            for k in (100, 10):
                items = r.search(user, query, k)
                ranks = [next((index+1 for index, m in enumerate(items) if text in m['content']), None)
                         for text in expected]
                r.quality.append({'scenario': i, 'category': category, 'top_k': k,
                                  'required_count': len(expected), 'evidence_ranks': ranks,
                                  'complete': all(rank is not None for rank in ranks),
                                  'result_count': len(items)})
        items = r.search(user, case['before_query'])
        r.check('historical_source_preserved_after_update', any(case['old'] in m['content'] for m in items))
        if i == 0:
            replay_before = {m['id']: m['content'] for m in items}
            r.check('replay_add_200', r.call('add', first_request)[0] == 200)
            replay_after = {m['id']: m['content'] for m in r.search(user, case['before_query'])}
            r.check('replay_does_not_duplicate_or_change_sources', replay_before == replay_after)
            conflict = {**first_request, 'messages': [{'role': 'user', 'content': 'different payload'}]}
            r.check('payload_conflict_409', r.call('add', conflict)[0] == 409)
            options = r.search(user, case['before_query'], options=['Unseen secret option', 'Other choice'])
            r.check('options_do_not_inject_evidence', {m['id']: m['content'] for m in options} == replay_after)
            r.check('top_k_one', len(r.search(user, case['before_query'], 1)) <= 1)
    unseen = f'eval:{run_id}:never-written'
    r.check('unseen_user_empty', r.search(unseen, 'parcel gate 包裹取货点') == [])
    # Same source text under a second identity must have independently scoped memory IDs.
    twin = {**first_request, 'user_id': unseen}
    r.check('second_user_add_200', r.call('add', twin)[0] == 200)
    a = {m['id'] for m in r.search(first_request['user_id'], 'Lin city')}
    b = {m['id'] for m in r.search(unseen, 'Lin city')}
    r.check('same_source_different_users_disjoint_ids', bool(a) and bool(b) and a.isdisjoint(b))
    r.check('second_user_no_first_user_later_session', not any('green gate' in m['content']
             for m in r.search(unseen, 'Lin parcel pickup')))
    search = {'query': 'test', 'user_id': unseen, 'top_k': 100}
    bad = [
        ('missing_auth', 'search', search, None, 401),
        ('wrong_auth', 'search', search, 'synthetic-invalid-key', 401),
        ('zero_top_k', 'search', {**search, 'top_k': 0}, 'configured', 422),
        ('excessive_top_k', 'search', {**search, 'top_k': 1001}, 'configured', 422),
        ('empty_query', 'search', {**search, 'query': ''}, 'configured', 422),
        ('invalid_role', 'add', {**first_request, 'messages': [{'role': 'system', 'content': 'text'}]}, 'configured', 422),
        ('non_text_content', 'add', {**first_request, 'messages': [{'role': 'user', 'content': []}]}, 'configured', 422),
        ('empty_messages', 'add', {**first_request, 'messages': []}, 'configured', 422),
        ('missing_field', 'add', {'user_id': unseen}, 'configured', 422),
        ('invalid_timestamp', 'add', {**first_request, 'messages': [{'role': 'user', 'content': 'text', 'timestamp': 1.5}]}, 'configured', 422),
    ]
    for name, kind, payload, key, expected in bad:
        status, _ = r.call(kind, payload, key)
        r.check(name, status == expected, {'expected': expected, 'actual': status})
    long_user = f'eval:{run_id}:long-source'
    long_text = '首段证据HEAD🧠' + ('中间长文记忆文本。' * 280) + '末段证据TAIL🧠'
    request = {'request_id': 'long', 'user_id': long_user, 'session_id': 'source',
               'messages': [{'role': 'user', 'content': long_text}]}
    r.check('long_source_add', r.call('add', request)[0] == 200)
    items = r.search(long_user, '首段证据 末段证据')
    r.check('long_source_head_and_tail', all(any(x in m['content'] for m in items)
            for x in ('首段证据HEAD🧠', '末段证据TAIL🧠')))
    r.check('long_source_multiple_chunks', len(items) > 1)
    source_items = [m for m in items if m['content'].startswith('[user]\n')]
    r.check('missing_time_explicitly_unknown', bool(source_items) and all(
        'Source time: unspecified.' in m['content'] for m in source_items))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--env-file', type=Path, default=ROOT / '.env')
    parser.add_argument('--base-url', default='http://localhost:8100')
    parser.add_argument('--timeout', type=float, default=120)
    parser.add_argument('--server-commit', required=True)
    parser.add_argument('--server-image', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if urllib.parse.urlsplit(args.base_url).hostname not in {'localhost', '127.0.0.1', '::1'}:
        parser.error('This synthetic runner is restricted to a local endpoint.')
    if args.env_file.exists():
        for line in args.env_file.read_text().splitlines():
            if '=' in line and not line.lstrip().startswith('#'):
                name, value = line.split('=', 1)
                os.environ.setdefault(name.strip(), value.strip().strip('\"\''))
    key = os.environ.get('MEMORIA_AML_API_KEY') or os.environ.get('AML_MEMORY_API_KEY')
    if not key:
        parser.error('MEMORIA_AML_API_KEY or AML_MEMORY_API_KEY must be configured.')
    run_id = 'aml-mini-' + uuid.uuid4().hex
    r = Runner(args.base_url, key, args.timeout)
    error = None
    try:
        execute(r, run_id)
    except Exception as exc:
        error = str(exc).replace(key, '<redacted>')
    groups = defaultdict(list)
    for q in r.quality:
        groups[f"{q['category']}@{q['top_k']}"].append(q)
    summary = {group: {'complete': sum(q['complete'] for q in rows), 'total': len(rows)}
               for group, rows in sorted(groups.items())}
    report = {'suite': 'aml-minimal-v1', 'run_id': run_id,
              'created_at': datetime.now(timezone.utc).isoformat(),
              'server_commit': args.server_commit, 'server_image': args.server_image,
              'runner_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'fixture_sha256': hashlib.sha256(json.dumps(scenarios(), ensure_ascii=False,
                                      sort_keys=True).encode()).hexdigest(),
              'unique_main_questions': 40, 'incremental_before_questions': 8,
              'protocol_assertions': len(r.checks), 'protocol_passed': sum(c['passed'] for c in r.checks),
              'quality_summary': summary, 'quality': r.quality, 'checks': r.checks,
              'http_status_counts': dict(Counter(str(e['status']) for e in r.events)),
              'latency': latency(r.events), 'events': r.events, 'error': error,
              'limits': ['Synthetic development data; not official AML samples or Smoke.',
                         'Evidence substring availability only; no Answer/Judge or temporal/state inference validation.',
                         'Each scenario has fewer than 100 records; Top100 completeness is a transport/availability check.',
                         'Top10 measures this small synthetic set, not product or leaderboard ranking quality.',
                         'No semantic relevance-order, branch/lifecycle, concurrency or retention-cleanup acceptance.',
                         'No retries or sleeps; synthetic users remain for reproducibility.']}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    args.output.chmod(0o600)
    print(json.dumps({k: report[k] for k in ('run_id', 'protocol_assertions', 'protocol_passed',
          'quality_summary', 'http_status_counts', 'latency', 'error')}, ensure_ascii=False, indent=2))
    print('Report:', args.output)
    return 1 if error or any(not c['passed'] for c in r.checks) or any(
        not q['complete'] for q in r.quality if q['top_k'] == 100) else 0


if __name__ == '__main__':
    raise SystemExit(main())
