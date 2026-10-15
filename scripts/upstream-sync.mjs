#!/usr/bin/env node
/** Run only in an ephemeral checkout. Never rewrites the maintained branch. */
import { execFileSync } from "node:child_process";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

function git(cwd, args, { raw = false } = {}) {
  const output = execFileSync(
    "git",
    ["-c", "core.hooksPath=/dev/null", ...args],
    {
      cwd,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      env: { ...process.env, GIT_TERMINAL_PROMPT: "0" },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  return raw ? output : output.trim();
}

function ancestor(cwd, before, after) {
  try {
    git(cwd, ["merge-base", "--is-ancestor", before, after]);
    return true;
  } catch (error) {
    if (error.status === 1) return false;
    throw error;
  }
}

function assertCandidate(branch) {
  if (!/^automation\/upstream-sync-\d+-\d+$/.test(branch)) {
    throw new Error("Refusing an unexpected candidate branch");
  }
}

function assertSha(sha) {
  if (!/^[0-9a-f]{40}$/.test(sha)) throw new Error("Invalid commit SHA");
}

function evidence(dir, name, value) {
  if (!dir) return;
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, name), value, { mode: 0o600 });
}

export function prepare({
  cwd,
  upstreamUrl,
  upstreamRef,
  candidateBranch,
  reportDir,
}) {
  assertCandidate(candidateBranch);
  if (!/^refs\/(heads|tags)\//.test(upstreamRef)) {
    throw new Error("Upstream must be a fully qualified branch or tag ref");
  }
  git(cwd, ["check-ref-format", upstreamRef]);
  if (git(cwd, ["status", "--porcelain"])) {
    throw new Error("Refusing to merge in a dirty checkout");
  }
  git(cwd, [
    "fetch",
    "--no-tags",
    "origin",
    "refs/heads/main:refs/remotes/origin/main",
  ]);
  git(cwd, ["switch", "--detach", "refs/remotes/origin/main"]);
  const base = git(cwd, ["rev-parse", "HEAD"]);
  git(cwd, ["fetch", "--no-tags", upstreamUrl, upstreamRef]);
  const upstream = git(cwd, ["rev-parse", "FETCH_HEAD^{commit}"]);
  const result = {
    status: "up_to_date",
    changed: false,
    base_sha: base,
    candidate_sha: base,
    candidate_branch: "",
    upstream_sha: upstream,
    upstream_ref: upstreamRef,
    conflicts: [],
  };
  if (ancestor(cwd, upstream, base)) return result;

  git(cwd, ["switch", "-c", candidateBranch]);
  try {
    git(cwd, [
      "-c",
      "user.name=github-actions[bot]",
      "-c",
      "user.email=41898282+github-actions[bot]@users.noreply.github.com",
      "-c",
      "commit.gpgsign=false",
      "merge",
      "--no-ff",
      "--no-edit",
      "-m",
      `Merge upstream ${upstreamRef}`,
      upstream,
    ]);
  } catch (error) {
    const conflicts = git(cwd, ["diff", "--name-only", "--diff-filter=U"])
      .split("\n")
      .filter(Boolean);
    evidence(
      reportDir,
      "merge-conflict.diff",
      git(cwd, ["diff", "--cc"], { raw: true }),
    );
    git(cwd, ["merge", "--abort"]);
    if (!conflicts.length) throw error;
    return { ...result, status: "conflict", conflicts };
  }
  const candidate = git(cwd, ["rev-parse", "HEAD"]);
  evidence(
    reportDir,
    "candidate.patch",
    git(cwd, ["diff", "--binary", base, candidate], { raw: true }),
  );
  // Unique per run/attempt; neither this push nor promotion ever uses --force.
  git(cwd, ["push", "origin", `${candidate}:refs/heads/${candidateBranch}`]);
  return {
    ...result,
    status: "prepared",
    changed: true,
    candidate_sha: candidate,
    candidate_branch: candidateBranch,
  };
}

export function promote({ cwd, baseSha, candidateSha, candidateBranch }) {
  assertSha(baseSha);
  assertSha(candidateSha);
  assertCandidate(candidateBranch);
  const refresh = () => {
    git(cwd, [
      "fetch",
      "--no-tags",
      "origin",
      "refs/heads/main:refs/remotes/origin/main",
    ]);
    return git(cwd, ["rev-parse", "refs/remotes/origin/main"]);
  };
  let current = refresh();
  git(cwd, ["fetch", "--no-tags", "origin", `refs/heads/${candidateBranch}`]);
  if (git(cwd, ["rev-parse", "FETCH_HEAD^{commit}"]) !== candidateSha) {
    throw new Error("Candidate changed after validation; refusing promotion");
  }
  if (!ancestor(cwd, baseSha, candidateSha)) {
    throw new Error("Candidate does not descend from the validated base");
  }
  if (ancestor(cwd, candidateSha, current))
    return { status: "already_published" };
  if (current !== baseSha) return { status: "deferred" };
  try {
    git(cwd, ["push", "origin", `${candidateSha}:refs/heads/main`]);
  } catch (error) {
    // A human push during CI is normal contention, not permission to overwrite it.
    current = refresh();
    if (ancestor(cwd, candidateSha, current))
      return { status: "already_published" };
    if (current !== baseSha) return { status: "deferred" };
    throw error;
  }
  return { status: "published" };
}

function publishResult(result, reportDir) {
  evidence(reportDir, "result.json", `${JSON.stringify(result, null, 2)}\n`);
  if (process.env.GITHUB_OUTPUT) {
    const fields = {
      ...result,
      conflict_files: JSON.stringify(result.conflicts ?? []),
    };
    delete fields.conflicts;
    // Multiline command diagnostics belong in result.json, not step outputs.
    delete fields.error;
    for (const [key, value] of Object.entries(fields)) {
      // Values originate from validated refs, SHAs, booleans or JSON serialization.
      if (/[\r\n]/.test(String(value)))
        throw new Error("Invalid multiline output");
      appendFileSync(process.env.GITHUB_OUTPUT, `${key}=${value}\n`);
    }
  }
  console.log(JSON.stringify(result));
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const reportDir = process.env.SYNC_REPORT_DIR;
  try {
    const result =
      process.argv[2] === "prepare"
        ? prepare({
            cwd: process.cwd(),
            upstreamUrl: "https://github.com/farion1231/cc-switch.git",
            upstreamRef: process.env.UPSTREAM_REF,
            candidateBranch: process.env.CANDIDATE_BRANCH,
            reportDir,
          })
        : process.argv[2] === "promote"
          ? promote({
              cwd: process.cwd(),
              baseSha: process.env.BASE_SHA,
              candidateSha: process.env.CANDIDATE_SHA,
              candidateBranch: process.env.CANDIDATE_BRANCH,
            })
          : (() => {
              throw new Error("Expected prepare or promote");
            })();
    publishResult(result, reportDir);
    if (result.status === "conflict") process.exitCode = 1;
  } catch (error) {
    // Never include the environment/credentials in diagnostic artifacts.
    publishResult({ status: "failed", error: error.message }, reportDir);
    console.error(error.message);
    process.exitCode = 1;
  }
}
