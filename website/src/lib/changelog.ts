// The changelog is the history of the repository, read at build time.
// It needs the full history: the CI checkout uses fetch-depth 0 for that.

import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export type CommitType =
  | 'feat'
  | 'fix'
  | 'docs'
  | 'test'
  | 'refactor'
  | 'perf'
  | 'build'
  | 'ci'
  | 'chore'
  | 'other';

export type Commit = {
  hash: string;
  short: string;
  date: string;
  type: CommitType;
  scope?: string;
  subject: string;
};

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');

const KNOWN_TYPES: CommitType[] = [
  'feat',
  'fix',
  'docs',
  'test',
  'refactor',
  'perf',
  'build',
  'ci',
  'chore',
];

function parseSubject(subject: string): { type: CommitType; scope?: string; subject: string } {
  const match = subject.match(/^(\w+)(?:\(([^)]+)\))?(!)?:\s*(.*)$/);
  if (!match) return { type: 'other', subject };
  const [, rawType, scope, bang, rest] = match;
  const type = KNOWN_TYPES.includes(rawType.toLowerCase() as CommitType)
    ? (rawType.toLowerCase() as CommitType)
    : 'other';
  return {
    type,
    scope,
    subject: bang ? `${rest}!` : rest,
  };
}

/** The commits of the repository, newest first. Empty when git is not available. */
export function getCommits(limit = 80): Commit[] {
  const format = '%H%x1f%h%x1f%aI%x1f%s%x1e';
  let raw = '';
  try {
    raw = execFileSync('git', ['log', `-${limit}`, `--format=${format}`], {
      cwd: repoRoot,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
      maxBuffer: 8 * 1024 * 1024,
    });
  } catch {
    return [];
  }

  return raw
    .split('\x1e')
    .map((record) => record.replace(/^\n+/, '').trim())
    .filter((record) => record.length > 0)
    .map((record) => {
      const [hash = '', short = '', date = '', subject = ''] = record.split('\x1f');
      const parsed = parseSubject(subject);
      return { hash, short, date, ...parsed };
    })
    .filter((commit) => commit.hash.length > 0);
}

/** The commits grouped by the month they were authored in. */
export function getCommitsByMonth(limit = 80): { month: string; commits: Commit[] }[] {
  const groups = new Map<string, Commit[]>();
  for (const commit of getCommits(limit)) {
    const month = commit.date.slice(0, 7);
    const list = groups.get(month) ?? [];
    list.push(commit);
    groups.set(month, list);
  }
  return [...groups.entries()].map(([month, commits]) => ({ month, commits }));
}
