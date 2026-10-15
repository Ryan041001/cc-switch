import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { prepare, promote } from "../upstream-sync.mjs";

function git(cwd, ...args) {
  return execFileSync("git", args, {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
  }).trim();
}
function author(cwd) {
  git(cwd, "config", "user.name", "Sync test");
  git(cwd, "config", "user.email", "sync-test@example.invalid");
  git(cwd, "config", "commit.gpgsign", "false");
  git(cwd, "config", "core.autocrlf", "false");
}
function commit(cwd, file, content) {
  writeFileSync(join(cwd, file), content);
  git(cwd, "add", file);
  git(cwd, "commit", "-m", `Change ${file}`);
  return git(cwd, "rev-parse", "HEAD");
}
function fixture(t, conflict = false) {
  const root = mkdtempSync(join(tmpdir(), "cc-switch-sync-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const upstream = join(root, "upstream");
  const origin = join(root, "origin.git");
  const work = join(root, "work");
  git(root, "init", "-b", "main", upstream);
  author(upstream);
  commit(upstream, "shared.txt", "base\n");
  git(upstream, "tag", "v1.0.0");
  git(root, "clone", "--bare", upstream, origin);
  git(root, "clone", origin, work);
  author(work);
  commit(work, "fork.txt", "Pi metrics\n");
  if (conflict) commit(work, "shared.txt", "fork change\n");
  git(work, "push", "origin", "main");
  const base = git(origin, "rev-parse", "main");
  // Include significant trailing spaces: the diagnostic patch must be lossless.
  commit(
    upstream,
    conflict ? "shared.txt" : "official.txt",
    "official change  \n",
  );
  git(upstream, "tag", "v1.1.0");
  const options = {
    cwd: work,
    upstreamUrl: upstream,
    upstreamRef: "refs/tags/v1.1.0",
    candidateBranch: "automation/upstream-sync-123-1",
    reportDir: join(root, "report"),
  };
  return { root, upstream, origin, work, base, options };
}
function publication(f, prepared) {
  return {
    cwd: f.work,
    baseSha: prepared.base_sha,
    candidateSha: prepared.candidate_sha,
    candidateBranch: prepared.candidate_branch,
  };
}

test("prepares a clean merge without publishing, then promotes only that exact SHA", (t) => {
  const f = fixture(t);
  const result = prepare(f.options);
  assert.equal(result.status, "prepared");
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
  assert.equal(
    git(f.origin, "show", `${result.candidate_sha}:fork.txt`),
    "Pi metrics",
  );
  assert.equal(
    git(f.origin, "show", `${result.candidate_sha}:official.txt`),
    "official change",
  );
  const verify = join(f.root, "verify");
  git(f.root, "clone", f.origin, verify);
  git(verify, "apply", "--check", join(f.options.reportDir, "candidate.patch"));
  assert.match(
    readFileSync(join(f.options.reportDir, "candidate.patch"), "utf8"),
    /official change  \n/,
  );
  assert.equal(promote(publication(f, result)).status, "published");
  assert.equal(git(f.origin, "rev-parse", "main"), result.candidate_sha);
  assert.equal(promote(publication(f, result)).status, "already_published");
});

test("an already included official release is a successful no-op", (t) => {
  const f = fixture(t);
  const result = prepare({ ...f.options, upstreamRef: "refs/tags/v1.0.0" });
  assert.equal(result.status, "up_to_date");
  assert.equal(result.changed, false);
  assert.equal(result.candidate_branch, "");
  assert.equal(result.candidate_sha, f.base);
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
});

test("conflicts are reported and aborted, never resolved with ours/theirs or published", (t) => {
  const f = fixture(t, true);
  const result = prepare(f.options);
  assert.equal(result.status, "conflict");
  assert.deepEqual(result.conflicts, ["shared.txt"]);
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
  assert.equal(git(f.work, "rev-parse", "HEAD"), f.base);
  assert.equal(git(f.work, "status", "--porcelain"), "");
  assert.match(
    readFileSync(join(f.options.reportDir, "merge-conflict.diff"), "utf8"),
    /fork change/,
  );
  assert.throws(() =>
    git(f.origin, "rev-parse", `refs/heads/${f.options.candidateBranch}`),
  );
});

test("a concurrent human change defers publication without overwriting either history", (t) => {
  const f = fixture(t);
  const result = prepare(f.options);
  const human = join(f.root, "human");
  git(f.root, "clone", f.origin, human);
  author(human);
  const advanced = commit(human, "human.txt", "keep this work\n");
  git(human, "push", "origin", "main");
  assert.equal(promote(publication(f, result)).status, "deferred");
  assert.equal(git(f.origin, "rev-parse", "main"), advanced);
  assert.equal(git(f.origin, "show", "main:human.txt"), "keep this work");
});

test("a candidate modified after CI cannot be promoted", (t) => {
  const f = fixture(t);
  const result = prepare(f.options);
  git(f.origin, "update-ref", `refs/heads/${result.candidate_branch}`, f.base);
  assert.throws(
    () => promote(publication(f, result)),
    /Candidate changed after validation/,
  );
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
});

test("dirty checkouts and invalid refs fail closed before any publication", (t) => {
  const f = fixture(t);
  assert.throws(
    () => prepare({ ...f.options, candidateBranch: "main" }),
    /unexpected candidate/,
  );
  assert.throws(
    () => prepare({ ...f.options, upstreamRef: "--upload-pack=bad" }),
    /fully qualified/,
  );
  writeFileSync(join(f.work, "uncommitted.txt"), "keep local work\n");
  assert.throws(() => prepare(f.options), /dirty checkout/);
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
});

test("a missing upstream ref is an error, not permission to reset main", (t) => {
  const f = fixture(t);
  assert.throws(() =>
    prepare({ ...f.options, upstreamRef: "refs/tags/missing" }),
  );
  assert.equal(git(f.origin, "rev-parse", "main"), f.base);
});
