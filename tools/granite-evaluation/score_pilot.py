"""Score a labelled fixture against completed native evidence; no model or DB access."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
from pilot_metrics import evaluate_query, read_vectors


def load_roster(input_path, vector_path, label_path):
    raw, labels_raw = input_path.read_bytes(), label_path.read_bytes()
    fixture, labels = json.loads(raw), json.loads(labels_raw)
    run = read_vectors(vector_path)
    if fixture['format_version'] != 1 or labels['format_version'] != 1:
        raise ValueError('unsupported fixture version')
    if run['start']['input_sha256'] != hashlib.sha256(raw).hexdigest():
        raise ValueError('native input SHA256 mismatch')
    samples = {r['id']: r['text'] for r in fixture['samples']}
    if len(samples) != len(fixture['samples']) or set(samples) != set(run['vectors']):
        raise ValueError('native input roster mismatch')
    ids = set()
    for row in labels['documents'] + labels['queries']:
        if row['id'] in ids or samples.get(row['id']) != row['text']:
            raise ValueError('labelled text/ID roster mismatch')
        if row['group'] not in ['memory', 'code']:
            raise ValueError('unsupported label group')
        ids.add(row['id'])
    for query in labels['queries']:
        targets = query['relevant_ids']
        candidates = {d['id'] for d in labels['documents'] if d['group'] == query['group']}
        if len(set(targets)) != len(targets) or not set(targets) <= candidates:
            raise ValueError('invalid/cross-group relevance labels')
    vectors = {}
    for identifier in ids:
        vector = run['vectors'][identifier]
        norm = math.sqrt(sum(v*v for v in vector))
        vectors[identifier] = [v/norm for v in vector]
    return labels, run, vectors, hashlib.sha256(labels_raw).hexdigest()


def score_pilot(input_path, vector_path, label_path, limit, server_floor, client_floor):
    labels, run, vectors, label_sha = load_roster(input_path, vector_path, label_path)
    result = dict(input_sha256=run['start']['input_sha256'], vectors_sha256=run['file_sha256'],
                  labels_sha256=label_sha, space=run['ready']['space'], limit=limit,
                  server_floor=server_floor, client_floor=client_floor, groups={})
    for group in ['memory', 'code']:
        documents = [d['id'] for d in labels['documents'] if d['group'] == group]
        queries = [q for q in labels['queries'] if q['group'] == group]
        positive, negative, all_results = [], [], []
        for query in queries:
            scores = {i: max(-1., min(1., sum(a*b for a,b in zip(vectors[i], vectors[query['id']])))) for i in documents}
            row = evaluate_query(scores, query['relevant_ids'], limit, server_floor, client_floor)
            (positive if query['relevant_ids'] else negative).append(row)
            all_results.append(row)
        if not positive or not negative:
            raise ValueError('each group requires answerable and unanswerable queries')
        returned = sum(r['returned'] for r in all_results)
        result['groups'][group] = dict(answerable_count=len(positive), unanswerable_count=len(negative),
            recall_bounds=[sum(r['recall_bounds'][j] for r in positive)/len(positive) for j in [0,1]],
            mrr_bounds=[sum(r['rr_bounds'][j] for r in positive)/len(positive) for j in [0,1]],
            micro_precision_bounds=[sum(r['relevant_returned_bounds'][j] for r in all_results)/returned for j in [0,1]] if returned else None,
            false_accepts=sum(r['false_accept'] for r in negative),
            answerable_abstentions=sum(r['abstained'] for r in positive))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['input', 'vectors', 'labels', 'output']:
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--limit', type=int, required=True)
    parser.add_argument('--server-floor', type=float, required=True)
    parser.add_argument('--client-floor', type=float, required=True)
    args = parser.parse_args()
    result = score_pilot(args.input, args.vectors, args.labels, args.limit,
                         args.server_floor, args.client_floor)
    descriptor = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, 'w') as output:
        json.dump(result, output, indent=2, allow_nan=False)
        output.write('\n')


if __name__ == '__main__':
    main()
