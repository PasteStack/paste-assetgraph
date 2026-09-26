Feature: JAM URL Generation
  As the paste-assetgraph render command
  I need to generate consistent combo URLs
  So that a versioned URL always names the exact group it serves

  Background:
    Given a manifest with the following assets:
      | key               | version | timestamp  | hash     |
      | js/paste/dom.js   | 1       | 1387575210 | abc123   |
      | js/paste/event.js | 2       | 1387575212 | def456   |
      | js/paste/ui/heroscroll.js | 3 | 1387575200 | ghi789 |

  Scenario: Generate combo URL for single module
    When I render a JAM JS group for modules:
      | paste.dom |
    Then the URL should be "/paste/1387575210/paste.dom+v1.0.min.js"

  Scenario: Generate combo URL for multiple modules
    When I render a JAM JS group for modules:
      | paste.dom   |
      | paste.event |
    Then the URL should be "/paste/1387575212/paste.dom+v1.0~paste.event+v2.0.min.js"
    And the timestamp should be the maximum of all module timestamps

  Scenario: Non-min URL form
    When a request names "/paste/1387575210/paste.dom+v1.0.js"
    Then it resolves to the same compiled output as the ".min.js" form

  Scenario: Generate CSS combo URL
    Given a manifest with the following assets:
      | key                  | version | timestamp  | hash   |
      | css/paste/base.css   | 1       | 1387575210 | aaa111 |
      | css/paste/ui.css     | 2       | 1387575215 | bbb222 |
    When I render a JAM CSS group for modules:
      | paste.base |
      | paste.ui   |
    Then the URL should be "/paste/1387575215/paste.base+v1.0~paste.ui+v2.0.min.css"

  Scenario: Modules absent from the manifest fail the render
    When I render a JAM JS group for modules:
      | paste.dom       |
      | nonexistent.mod |
    Then the render fails with an error naming "nonexistent.mod" and "js"

  Scenario: URL with CDN base
    When I render a JAM JS group for modules with cdnBase="//cdn.example.com":
      | paste.dom |
    Then the URL should start with "//cdn.example.com"
    And the path should be "/paste/1387575210/paste.dom+v1.0.min.js"

  Scenario: Modules ordered by dependency, not request order
    Given "paste.event" requires "paste.dom" and "paste.dom" requires "paste.util"
    When I render a JAM JS group for modules:
      | paste.event |
      | paste.util  |
      | paste.dom   |
    Then the URL module order should be "paste.util~paste.dom~paste.event"
    And NOT the request order
