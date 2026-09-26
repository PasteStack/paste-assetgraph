Feature: JAM Combo Serving
  As the paste-assetgraph combo command
  I need to resolve combo URLs to compiled responses
  So that exact canonical groups cache immutably and everything else
  redirects or stays fresh — the host framework writes the HTTP

  Background:
    Given a manifest with modules and compiled output under the output directory:
      | module        | dependencies |
      | paste         |              |
      | paste.dom     | paste        |
      | paste.event   | paste.dom    |
      | site.theme    |              |
    And every module's current version carries its build stamp

  Scenario: Versioned JS URL serves the exact named group
    When I request "/paste/1770462000/paste.dom+v2.0~paste.event+v1.0.min.js"
    Then the status is 200
    And the body is the compiled output for "paste.dom" then "paste.event", joined by "//;"
    And no unlisted dependency is added

  Scenario: Versioned CSS URL
    When I request "/paste/1770465409/site.theme+v1.0.min.css"
    Then the status is 200
    And the body is the compiled CSS output
    And the content type is "text/css"

  Scenario: Versioned response caches for a year
    When I request "/paste/1770462000/paste.dom+v2.0.min.js"
    Then the Cache-Control header is "public, max-age=31536000"
    And the response is marked cacheable

  Scenario: Dependencies are not expanded without require
    When I request "/paste/1770462000/paste.event+v1.0.min.js"
    Then the body contains only the "paste.event" output
    And "paste.dom" and "paste" are not served

  Scenario: /require/ path expands dependencies
    When I request "/paste/require/paste.event.min.js"
    Then the body is "paste", "paste.dom" and "paste.event" in dependency order
    And the Cache-Control header is "no-cache"

  Scenario: ?require query expands dependencies
    When I request "/paste/1770462000/paste.event+v1.0.min.js?require"
    Then the body is "paste", "paste.dom" and "paste.event" in dependency order
    And the Cache-Control header is "no-cache"

  Scenario: ?filter= removes delivered names after ordering
    When I request "/paste/1770462000/paste.dom+v2.0~paste.event+v1.0.min.js?require&filter=paste.dom"
    Then "paste.dom" is dropped from the served group
    And "paste" and "paste.event" remain in dependency order

  Scenario: Comma separators are accepted in lists and filter values
    When I request "/paste/1770462000/paste.dom+v2.0,paste.event+v1.0.min.js?filter=paste,paste.dom"
    Then the list and the filter both parse as multiple names

  Scenario: Unversioned URL serves the named group with no-cache
    When I request "/paste/paste.dom~paste.event.min.js"
    Then the status is 200 with the named modules in dependency order
    And the Cache-Control header is "no-cache"

  Scenario: Non-min suffix serves the same compiled output
    When I request "/paste/1770462000/paste.dom+v2.0.js"
    Then the body equals the ".min.js" response for the same group
    And the Cache-Control header is "public, max-age=31536000"

  Scenario: The stamp segment is the group's latest build stamp
    Given the listed modules' stamps are "1770462000" and "1770465409"
    When the canonical URL for the group is rendered
    Then the segment is "1770465409" — the maximum

  Scenario: An older stamp redirects to the canonical URL
    Given the canonical URL is "/paste/1770465409/paste.dom+v2.0.min.js"
    When I request "/paste/1770462000/paste.dom+v2.0.min.js"
    Then the status is 302
    And the Location header is "/paste/1770465409/paste.dom+v2.0.min.js"
    And the Cache-Control header is "no-cache"
    And the body is empty

  Scenario: A non-numeric segment redirects to the canonical URL
    When I request "/paste/static/paste.dom+v2.0.min.js"
    Then the status is 302 to the group's canonical stamped URL
    And the Cache-Control header is "no-cache"

  Scenario: A newer stamp is not-found
    When I request "/paste/9999999999/paste.dom+v2.0.min.js"
    Then the status is 404 with body "/* modules not found */"
    And the Cache-Control header is "no-cache"

  Scenario: A listed order that is not the dependency order redirects
    Given "paste.event" depends on "paste.dom"
    When I request "/paste/1770465409/paste.event+v1.0~paste.dom+v2.0.min.js"
    Then the status is 302
    And the Location names "paste.dom+v2.0~paste.event+v1.0" in that order

  Scenario: A redirect preserves the request's query string
    When I request "/paste/1770462000/paste.dom+v2.0.min.js?filter=paste"
    Then the status is 302
    And the Location ends with "?filter=paste"

  Scenario: An unpinned name under a stamp redirects to its pinned URL
    When I request "/paste/1770462000/paste.dom.min.js"
    Then the status is 302
    And the Location names "paste.dom" pinned at its current version

  Scenario: Missing module returns the not-found response
    When I request "/paste/1770462000/nonexistent+v1.0.min.js"
    Then the status is 404
    And the body is "/* modules not found */"
    And the Cache-Control header is "no-cache"

  Scenario: One unknown module in a group sinks the whole response
    When I request "/paste/1770462000/paste.dom+v2.0~nonexistent+v1.0.min.js"
    Then the status is 404
    And the body is "/* modules not found */"
    And the Cache-Control header is "no-cache"
    And the response is not cacheable

  Scenario: An invalid or empty module name in a group sinks the whole response
    When I request "/paste/1770462000/paste.dom+v2.0~bad%20name+v1.0.min.js"
    Then the status is 404 with body "/* modules not found */"
    And the response is not cacheable

  Scenario: ?require expansion does not rescue an unknown module
    When I request "/paste/1770462000/paste.dom+v2.0~nonexistent+v1.0.min.js?require"
    Then the status is 404 with body "/* modules not found */"

  Scenario: Module without readable compiled output returns not-found
    Given a manifest module whose output file is absent or unreadable
    When I request its URL
    Then the status is 404 with body "/* modules not found */"

  Scenario: A group containing a module without readable output is not-found
    Given a manifest module whose output file is absent or unreadable
    When I request a versioned URL naming it together with a readable module
    Then the status is 404 with body "/* modules not found */"
    And the Cache-Control header is "no-cache"

  Scenario: A retained history version serves its own content
    Given the manifest keeps "paste.dom" version 1 in history with its compiled file
    When I request the pinned URL with that version's own stamp
    Then the status is 200
    And the body is "paste.dom"'s version 1 compiled output
    And the Cache-Control header is "public, max-age=31536000"
    And the response is marked cacheable

  Scenario: Historical pins keep the listed order
    Given "paste.dom" was rebuilt so the dependency order changed since a page was rendered
    When I request that page's URL with "paste.dom" pinned at its retained version
    Then the status is 200
    And the group serves in the order the URL lists, not today's layering

  Scenario: A version newer than the manifest knows is not-found
    When I request "/paste/1770462000/paste.dom+v9.0.min.js"
    Then the status is 404 with body "/* modules not found */"
    And the Cache-Control header is "no-cache"
    And the response is not cacheable

  Scenario: A history version whose compiled file was pruned is not-found
    Given the manifest keeps "paste.dom" version 1 in history but its file is gone
    When I request the pinned URL with that version's own stamp
    Then the status is 404 with body "/* modules not found */"
    And the Cache-Control header is "no-cache"

  Scenario: The runtime loader's packages URL serves the current build
    Given the manifest's packages segment is "71f8e72a3d6b"
    When I request "/paste/71f8e72a3d6b/paste.dom.js"
    Then the status is 200 with "paste.dom"'s current output
    And the Cache-Control header is "public, max-age=31536000"

  Scenario: A stale loader segment redirects to the pinned URL
    Given the manifest's packages segment is "71f8e72a3d6b"
    When I request "/paste/deadbeef0000/paste.dom.js"
    Then the status is 302 to "paste.dom"'s canonical pinned URL
    And the Cache-Control header is "no-cache"

  Scenario: Serving is compiled output only
    When I request any combo URL
    Then the body is read from compiled files under the output directories
    And no source files are served
    And no SCSS is compiled at request time
