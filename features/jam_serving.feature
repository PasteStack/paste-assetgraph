Feature: JAM Combo URL Serving
  As a paste-assetgraph server (PasteComboHandler)
  I need to serve concatenated module content
  So that browsers can load multiple modules in a single request

  Background:
    Given module files:
      | path                  | content                          |
      | js/paste/dom.js       | paste.define('paste.dom',[],fn); |
      | js/paste/event.js     | paste.define('paste.event',[],fn);|
      | css/paste/base.css    | body { margin: 0; }              |
      | css/paste/ui.css      | .ui { display: block; }          |

  Scenario: Serve JS combo URL
    When I request "/paste/1387575212/paste.dom+v1.0,paste.event+v1.0.min.js"
    Then the response status should be 200
    And the Content-Type should be "application/javascript"
    And the response body should contain "paste.define('paste.dom'"
    And the response body should contain "paste.define('paste.event'"
    And the paste.dom content should appear before paste.event content

  Scenario: Serve CSS combo URL
    When I request "/paste/1387575215/paste.base+v1.0,paste.ui+v1.0.min.css"
    Then the response status should be 200
    And the Content-Type should be "text/css"
    And the response body should contain "body { margin: 0; }"
    And the response body should contain ".ui { display: block; }"

  Scenario: Cache headers based on timestamp
    When I request "/paste/1387575212/paste.dom+v1.0.min.js"
    Then the Cache-Control header should allow long-term caching
    And the ETag header should be present

  Scenario: 404 for non-existent module
    When I request "/paste/1387575212/nonexistent+v1.0.min.js"
    Then the response status should be 404

  Scenario: Parse module names from URL
    Given URL "/paste/1387575212/paste.dom+v1.0,paste.event+v2.0,paste.ui.heroscroll+v3.0.min.js"
    When I parse the modules from the URL
    Then the modules should be:
      | module              | version |
      | paste.dom           | 1       |
      | paste.event         | 2       |
      | paste.ui.heroscroll | 3       |

  Scenario: Handle debug URLs
    When I request "/paste/1387575212/paste.dom+v1.0.js"
    Then the response should contain unminified content
    And source maps may be included

  Scenario: Validate timestamp in URL
    Given current manifest timestamp is 1387575212
    When I request "/paste/1387575210/paste.dom+v1.0.min.js"
    Then the response should still serve the content
    And the response should use the requested timestamp for caching

  Scenario: Graceful handling of partially missing modules
    When I request "/paste/1387575212/paste.dom+v1.0,nonexistent+v1.0.min.js"
    Then the response status should be 200
    And the response should contain only paste.dom content
    And a warning should be logged for the missing module

  Scenario: Debug mode serves raw source files
    Given module source files:
      | source_path           | content                               |
      | src/js/paste/dom.js   | /* raw paste.dom source */            |
    When I request "/paste/1387575212/paste.dom+v1.0.js"
    Then the response should contain "/* raw paste.dom source */"
    And the content should NOT be minified

  Scenario: Production mode serves compiled files
    Given compiled module files:
      | output_path                    | content                    |
      | js/paste.dom.abc123.v1.min.js  | paste.define("paste.dom"); |
    When I request "/paste/1387575212/paste.dom+v1.0.min.js"
    Then the response should contain "paste.define(\"paste.dom\");"
    And the content should be from the compiled output

  Scenario: SCSS compiled on-the-fly in debug mode
    Given SCSS module files:
      | source_path                 | content                        |
      | modules/hero/heroscroll.scss | .hero { background: red; }     |
    And no compiled CSS exists for paste.ui.heroscroll
    When I request "/paste/1387575212/paste.ui.heroscroll+v1.0.css"
    Then the SCSS should be compiled to CSS
    And the response should contain ".hero"
    And the Content-Type should be "text/css"

  Scenario: Modules served in sorted_deps order
    Given a manifest with sorted_deps:
      | name          | path              | version |
      | paste.util    | js/paste/util.js  | 1       |
      | paste.dom     | js/paste/dom.js   | 1       |
      | paste.event   | js/paste/event.js | 2       |
    When I request "/paste/123/paste.event+v2.0,paste.util+v1.0.min.js"
    Then the response should contain paste.util content BEFORE paste.event content
    And the order should follow sorted_deps, not request order

  Scenario: Expand transitive dependencies when serving
    Given module paste.event depends on paste.dom
    And module paste.dom depends on paste.util
    When I request with require_dependencies=true for "paste.event"
    Then the response should include paste.util, paste.dom, and paste.event
    And they should be in dependency order
