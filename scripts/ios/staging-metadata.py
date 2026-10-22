#!/usr/bin/env python3
"""Read-only App Store Connect build allocation/readback for Crew Staging."""
import json
import os
from pathlib import Path
import re
import sys
import time
from urllib.parse import urlencode, urlparse
from urllib.request import Request, urlopen

BUNDLE = "ai.ashler.crew.staging"
API = "https://api.appstoreconnect.apple.com"


def next_build(builds, floor):
    versions = [row["attributes"]["version"] for row in builds]
    if any(not re.fullmatch(r"[0-9]+", value) for value in versions):
        raise ValueError("Non-integer staging build metadata requires explicit release review")
    return max([floor - 1] + [int(value) for value in versions]) + 1


def request(path):
    import jwt
    url = path if path.startswith("https://") else API + path
    if urlparse(url).netloc != "api.appstoreconnect.apple.com" or urlparse(url).scheme != "https":
        raise ValueError("Refusing untrusted App Store Connect pagination URL")
    now = int(time.time())
    token = jwt.encode({"iss": os.environ["AC_API_ISSUER_ID"], "iat": now,
                        "exp": now + 1199, "aud": "appstoreconnect-v1"},
                       Path(os.environ["AC_KEY_PATH"]).read_bytes(), algorithm="ES256",
                       headers={"kid": os.environ["AC_API_KEY_ID"], "typ": "JWT"})
    with urlopen(Request(url, headers={"Authorization": "Bearer " + token}), timeout=60) as response:
        return json.load(response)


def rows(path):
    result = []
    seen = set()
    while path:
        if path in seen:
            raise ValueError("Repeated App Store Connect pagination URL")
        seen.add(path)
        page = request(path)
        result.extend(page["data"])
        path = page.get("links", {}).get("next")
    return result


def main():
    mode = sys.argv[1]
    if mode == "self-check":
        assert next_build([], 37) == 37
        assert next_build([{"attributes": {"version": "52"}}, {"attributes": {"version": "36"}}], 37) == 53
        try:
            next_build([{"attributes": {"version": "1.2"}}], 37)
        except ValueError:
            pass
        else:
            raise AssertionError("ambiguous version accepted")
        return
    apps = rows("/v1/apps?" + urlencode({"filter[bundleId]": BUNDLE, "limit": 200}))
    if len(apps) != 1 or apps[0]["attributes"]["bundleId"] != BUNDLE:
        raise ValueError("Expected exactly the existing Crew Staging App Store Connect app")
    app_id = apps[0]["id"]
    query = "/v1/builds?" + urlencode({"filter[app]": app_id, "limit": 200})
    if mode == "next":
        builds = rows(query)
        number = next_build(builds, int(sys.argv[2]))
        print(json.dumps({"bundleId": BUNDLE, "appId": app_id, "buildNumber": number,
                          "observedBuildNumbers": [b["attributes"]["version"] for b in builds]}, indent=2))
    elif mode == "readback":
        number = sys.argv[2]
        for _ in range(40):
            builds = [b for b in rows(query) if b["attributes"]["version"] == number]
            if len(builds) > 1:
                raise ValueError("Ambiguous staging build readback")
            if builds:
                build = builds[0]
                state = build["attributes"]["processingState"]
                if state in ("FAILED", "INVALID"):
                    raise ValueError("Apple rejected staging build: " + state)
                if state == "VALID":
                    beta = request("/v1/builds/" + build["id"] + "/buildBetaDetail")["data"]
                    groups = rows("/v1/builds/" + build["id"] + "/betaGroups?limit=200")
                    print(json.dumps({"bundleId": BUNDLE, "appId": app_id, "build": build,
                                      "betaDetail": beta, "groups": groups}, indent=2))
                    return
            time.sleep(15)
        raise TimeoutError("Staging build has not completed Apple processing")
    else:
        raise ValueError("Use next FLOOR, readback BUILD, or self-check")


if __name__ == "__main__":
    main()
