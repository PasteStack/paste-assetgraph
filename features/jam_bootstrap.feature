Feature: JAM Bootstrap Generation
  As a paste-assetgraph runtime
  I need to generate consistent bootstrap HTML
  So that paste.js module loading works correctly across all runtimes

  Background:
    Given a manifest with the following assets:
      | key               | version | timestamp  |
      | js/paste/dom.js   | 1       | 1387575210 |
      | js/paste/event.js | 2       | 1387575212 |

  Scenario: Generate bootstrap with isLoading pre-registration
    When I generate a bootstrap for modules:
      | paste.dom   |
      | paste.event |
    Then the script should contain "d('paste.dom').isLoading"
    And the script should contain "d('paste.event').isLoading"
    And the isLoading assignments should be chained with "="
    And the chain should end with "=!0"

  Scenario: Bootstrap uses paste.require to load combo URL
    When I generate a bootstrap for modules:
      | paste.dom |
    Then the script should call "r('/paste/..."
    And the script should use "(paste.define,paste.require)"

  Scenario: Generate inline bootstrap with paste.js content
    Given paste.js file at "/tmp/paste.js" with content:
      """
      (function(w){w.paste={define:function(){}}})(window);
      """
    When I generate an inline bootstrap with pasteJsPath="/tmp/paste.js" for modules:
      | paste.dom |
    Then the output should contain two script tags
    And the first script tag should contain the paste.js content
    And the second script tag should contain the module loading bootstrap

  Scenario: Inline bootstrap reads from classpath resource
    Given paste.js resource at "/paste.js" on classpath with content:
      """
      /* paste.js from classpath */
      """
    When I generate an inline bootstrap with pasteJsPath="/paste.js" for modules:
      | paste.dom |
    Then the first script tag should contain "/* paste.js from classpath */"

  Scenario: Fallback when paste.js not found
    When I generate an inline bootstrap with pasteJsPath="/nonexistent.js" for modules:
      | paste.dom |
    Then the output should use standard bootstrap (not inline)
    And the script should contain "paste.require"

  Scenario: Bootstrap with CDN base
    When I generate a bootstrap with cdnBase="//cdn.example.com" for modules:
      | paste.dom |
    Then the combo URL in the script should start with "//cdn.example.com"

  Scenario: Bootstrap includes transitive dependencies
    Given module paste.event has expanded dependencies:
      | paste.util |
      | paste.dom  |
    When I generate a bootstrap for modules:
      | paste.event |
    Then the isLoading assignments should include all dependencies
    And the script should contain "d('paste.util').isLoading"
    And the script should contain "d('paste.dom').isLoading"
    And the script should contain "d('paste.event').isLoading"

  Scenario: Bootstrap modules in sorted_deps order
    Given a manifest with sorted_deps:
      | name          | version |
      | paste.util    | 1       |
      | paste.dom     | 1       |
      | paste.event   | 2       |
    When I generate a bootstrap for modules:
      | paste.event |
      | paste.util  |
    Then the combo URL modules should be in sorted_deps order
    And NOT the request order

  Scenario: Inline bootstrap sets packagesUrl for dynamic loading
    Given paste.js file at "/tmp/paste.js"
    When I generate an inline bootstrap for modules:
      | paste.dom |
    Then the output should include paste.config call
    And packagesUrl should be set to the combo base URL
