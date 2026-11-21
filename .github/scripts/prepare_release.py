"""Validate an existing version tag and collect bounded, author-free release facts."""

import argparse
import json
from pathlib import Path
import re
import subprocess
import tomllib


VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-((?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)
MAX_COMMITS = 100
MAX_SUBJECT = 240
MAX_FACT_BYTES = 32 * 1024


def git(root: Path, *arguments: str) -> str:
    return subprocess.check_output(
        ["git", "-C", str(root), *arguments], encoding="utf-8", errors="replace",
        stderr=subprocess.PIPE,
    ).strip()


def public_subject(subject: str) -> str:
    # Never include author identities, mentions, credential-looking values, or links.
    subject = re.sub(r"[^\s<>@]+@[^\s<>@]+", "[email omitted]", subject)
    subject = re.sub(r"https?://\S+", "[link omitted]", subject)
    subject = re.sub(r"@[A-Za-z0-9_-]+", "[mention omitted]", subject)
    subject = re.sub(
        r"(?i)\b(?:sk-|gh[pousr]_)[A-Za-z0-9_-]{8,}", "[credential omitted]", subject
    )
    subject = "".join(character for character in subject if character.isprintable() or character.isspace())
    subject = " ".join(subject.split())
    return subject[:MAX_SUBJECT]


def previous_tag(root: Path, source: str, current: str) -> str | None:
    excluded = [current]
    # Ignore invalid tags and other tags on the same commit, without unbounded work.
    for _ in range(32):
        try:
            tag = git(
                root, "describe", "--tags", "--abbrev=0", "--match=v[0-9]*",
                *(f"--exclude={name}" for name in excluded), source,
            )
        except subprocess.CalledProcessError:
            return None
        if VERSION.fullmatch(tag.removeprefix("v")) and git(
            root, "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"
        ) != source:
            return tag
        excluded.append(tag)
    return None


def prepare(root: Path, tag: str, expected_sha: str) -> dict:
    if len(tag) > 128 or not tag.startswith("v") or not VERSION.fullmatch(tag[1:]):
        raise ValueError("release tag must be v<SemVer>, for example v0.1.1")
    if re.fullmatch(r"[0-9a-f]{40}", expected_sha) is None:
        raise ValueError("expected source must be a full Git object ID")
    source = git(root, "rev-parse", "HEAD")
    if source != git(root, "rev-parse", "--verify", f"{expected_sha}^{{commit}}"):
        raise ValueError("checkout does not match the triggering Git object")
    if source != git(root, "rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}"):
        raise ValueError("release tag does not point at the checked-out commit")
    try:
        git(root, "merge-base", "--is-ancestor", source, "refs/remotes/origin/main")
    except subprocess.CalledProcessError as error:
        raise ValueError("release commit must belong to origin/main history") from error
    manifest = tomllib.loads(git(root, "show", f"{source}:Cargo.toml"))
    version = manifest["workspace"]["package"]["version"]
    if tag != f"v{version}":
        raise ValueError("release tag must match workspace.package.version")
    previous = previous_tag(root, source, tag)
    revision = f"{previous}..{source}" if previous else source
    count = int(git(root, "rev-list", "--count", revision))
    lines = git(
        root, "log", "--no-decorate", f"--max-count={MAX_COMMITS}",
        "--format=%h%x09%s", revision,
    ).splitlines()
    commits = []
    for line in lines:
        sha, separator, subject = line.partition("\t")
        if not separator or re.fullmatch(r"[0-9a-f]{7,40}", sha) is None:
            raise ValueError("unexpected Git log record")
        commits.append({"sha": sha, "subject": public_subject(subject)})
    facts = {
        "tag": tag, "version": version, "source_sha": source,
        "prerelease": VERSION.fullmatch(version).group(4) is not None,
        "previous_tag": previous, "commit_count": count, "commits": commits,
    }
    # Multi-byte subjects must not turn a valid release into an oversized request.
    while len(json.dumps(facts, ensure_ascii=False, indent=2).encode("utf-8")) > MAX_FACT_BYTES:
        if not commits:
            raise ValueError("release metadata exceeds the size limit")
        commits.pop()
    return facts


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--tag", required=True)
    parser.add_argument("--expected-sha", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    try:
        facts = prepare(arguments.root, arguments.tag, arguments.expected_sha)
        with arguments.output.open("x", encoding="utf-8", newline="\n") as output:
            json.dump(facts, output, ensure_ascii=False, indent=2)
            output.write("\n")
    except (ValueError, KeyError, subprocess.CalledProcessError) as error:
        detail = str(error) if isinstance(error, ValueError) else type(error).__name__
        parser.exit(1, f"Release validation failed: {detail}.\n")


if __name__ == "__main__":
    main()
