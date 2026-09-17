"""Identify installer tooling consistently to the public download service."""
import urllib.request

USER_AGENT = "raft-computer-installer/0.3.0 (release tooling)"


def public_request(url):
    return urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
