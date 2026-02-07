Feature: JAM URL Generation
  As a paste-assetgraph runtime
  I need to generate consistent combo URLs
  So that all runtimes produce identical URLs for the same modules

  Background:
    Given a manifest with the following assets:
      | key               | version | timestamp  | hash     |
      | js/paste/dom.js   | 1       | 1387575210 | abc123   |
      | js/paste/event.js | 2       | 1387575212 | def456   |
      | js/paste/ui/heroscroll.js | 3 | 1387575200 | ghi789 |

  Scenario: Generate combo URL for single module
    When I generate a JAM JS URL for modules:
      | paste.dom |
    Then the URL should be "/paste/1387575210/paste.dom+v1.0.min.js"

  Scenario: Generate combo URL for multiple modules
    When I generate a JAM JS URL for modules:
      | paste.dom   |
      | paste.event |
    Then the URL should be "/paste/1387575212/paste.dom+v1.0,paste.event+v2.0.min.js"
    And the timestamp should be the maximum of all module timestamps

  Scenario: Generate debug URL
    When I generate a JAM JS URL for modules with debug=true:
      | paste.dom |
    Then the URL should be "/paste/1387575210/paste.dom+v1.0.js"
    And the URL should NOT contain ".min"

  Scenario: Generate CSS combo URL
    Given a manifest with the following assets:
      | key                  | version | timestamp  | hash   |
      | css/paste/base.css   | 1       | 1387575210 | aaa111 |
      | css/paste/ui.css     | 2       | 1387575215 | bbb222 |
    When I generate a JAM CSS URL for modules:
      | paste.base |
      | paste.ui   |
    Then the URL should be "/paste/1387575215/paste.base+v1.0,paste.ui+v2.0.min.css"

  Scenario: Handle missing modules gracefully
    When I generate a JAM JS URL for modules:
      | paste.dom       |
      | nonexistent.mod |
    Then the URL should only include existing modules
    And the URL should be "/paste/1387575210/paste.dom+v1.0.min.js"

  Scenario: URL with CDN base
    When I generate a JAM JS URL for modules with cdnBase="//cdn.example.com":
      | paste.dom |
    Then the URL should start with "//cdn.example.com"
    And the path should be "/paste/1387575210/paste.dom+v1.0.min.js"

  Scenario: Star dependency expansion
    Given a manifest with sorted_deps:
      | name          | path              | version |
      | paste.util    | js/paste/util.js  | 1       |
      | paste.dom     | js/paste/dom.js   | 1       |
      | paste.event   | js/paste/event.js | 2       |
    When I generate a JAM JS URL for modules:
      | paste.* |
    Then the URL should include all paste modules
    And the modules should be in sorted_deps order

  Scenario: Modules ordered by sorted_deps
    Given a manifest with sorted_deps:
      | name          | path              | version |
      | paste.util    | js/paste/util.js  | 1       |
      | paste.dom     | js/paste/dom.js   | 1       |
      | paste.event   | js/paste/event.js | 2       |
    When I generate a JAM JS URL for modules:
      | paste.event |
      | paste.util  |
      | paste.dom   |
    Then the URL module order should be "paste.util,paste.dom,paste.event"
    And NOT the request order
