"""Cosine-only fixture metrics; no inference, DB access or production configuration edits."""
import hashlib
import json
import math
import re


def validate_questions(records, questions):
    ids = {r['id'] for r in records}
    if any(q['target'] not in ids for q in questions):
        raise ValueError('question target is absent from this frozen corpus')


def evaluate_query(scores, relevant, limit, server_floor, client_floor):
    numeric = lambda n: isinstance(n, (int, float)) and not isinstance(n, bool) and math.isfinite(n) and -1 <= n <= 1
    if type(limit) is not int or limit < 1 or not all(numeric(f) for f in (server_floor, client_floor)):
        raise ValueError('invalid limit/floors')
    if any(not isinstance(i, str) or not i or not numeric(s) for i, s in scores.items()) or not set(relevant) <= scores.keys():
        raise ValueError('invalid score/label roster')
    relevant = set(relevant)
    ranked = sorted(scores.values(), reverse=True)
    floor = max(server_floor, client_floor)
    cap = min(limit, len(ranked))
    returned = low = high = 0
    if cap:
        boundary = ranked[cap-1]
        definite = [i for i, s in scores.items() if s > boundary and s >= floor]
        low = high = sum(i in relevant for i in definite)
        returned = len(definite)
        if boundary >= floor:
            tied = [i for i, s in scores.items() if s == boundary]
            slots = cap - sum(s > boundary for s in scores.values())
            positives = sum(i in relevant for i in tied)
            low += max(0, slots - (len(tied) - positives))
            high += min(slots, positives)
            returned += slots
    rr = None
    unique = False
    if relevant:
        best = max(scores[i] for i in relevant)
        above = sum(s > best for s in scores.values())
        negative_ties = sum(s == best and i not in relevant for i, s in scores.items())
        first, last = above+1, above+negative_ties+1
        rr = [1/last if best >= floor and last <= cap else 0,
              1/first if best >= floor and first <= cap else 0]
        unique = best >= floor and above == 0 and sum(s == best for s in scores.values()) == 1
    return dict(returned=returned, precision_bounds=[low/returned, high/returned] if returned else None,
                recall_bounds=[low/len(relevant), high/len(relevant)] if relevant else None,
                relevant_returned_bounds=[low, high], rr_bounds=rr, unique_top1=unique,
                abstained=returned == 0, false_accept=not relevant and returned > 0)


def read_vectors(path):
    raw = path.read_bytes()
    try:
        rows = [json.loads(line) for line in raw.splitlines()]
        if len(rows) < 3 or rows[0]['type'] != 'start' or rows[1]['type'] != 'ready' or rows[-1]['type'] != 'complete':
            raise ValueError('native run is incomplete')
        start, ready, end = rows[0], rows[1], rows[-1]
        if type(start['format_version']) is not int or start['format_version'] != 1 or not re.fullmatch('[0-9a-f]{64}', start['input_sha256']):
            raise ValueError('invalid native provenance')
        dims = ready['space']['dimensions']
        if type(dims) is not int or dims < 1:
            raise ValueError('invalid native dimensions')
        vectors = {}
        for row in rows[2:-1]:
            identifier, vector = row['id'], row['vector']
            if row['type'] != 'vector' or not isinstance(identifier, str) or not identifier.strip() or identifier in vectors:
                raise ValueError('invalid/duplicate native row')
            if len(vector) != dims or any(not isinstance(v, (int,float)) or isinstance(v,bool) or not math.isfinite(v) for v in vector):
                raise ValueError('invalid native vector')
            if abs(math.sqrt(sum(v*v for v in vector))-1) > 1e-5:
                raise ValueError('nonunit native vector')
            vectors[identifier] = vector
        if type(start['sample_count']) is not int or type(end['sample_count']) is not int or not vectors or start['sample_count'] != len(vectors) or end['sample_count'] != len(vectors):
            raise ValueError('native roster is incomplete')
    except (KeyError, TypeError, json.JSONDecodeError) as error:
        raise ValueError('malformed native evidence') from error
    return dict(vectors=vectors, ready=ready, start=start, rows=rows[2:-1], file_sha256=hashlib.sha256(raw).hexdigest())
