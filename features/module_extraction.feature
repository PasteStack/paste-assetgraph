Feature: Module Name Extraction
  As a paste-assetgraph build tool
  I need to extract module names from @module comments
  So that modules can be served by their declared names instead of filename conventions

  Background:
    Given the assetgraph configuration:
      | source_js   | src/js   |
      | source_scss | src/scss |

  Scenario: Extract @module from JavaScript file
    Given a JavaScript file "src/js/site.js" with content:
      """
      /**
       * Site module
       * @requires paste/ui/heroscroll
       * @module site/home
       */
      paste.define('site.home', ['paste.ui.heroscroll'], function() {});
      """
    When I build the asset manifest
    Then the manifest modules should contain:
      | module_name | file_path      |
      | site.home    | js/site.js     |

  Scenario: Extract @module from SCSS file
    Given an SCSS file "src/scss/theme.scss" with content:
      """
      /**
       * Theme styles
       * @module acme/theme
       */
      body { color: #333; }
      """
    When I build the asset manifest
    Then the manifest modules should contain:
      | module_name  | file_path       |
      | acme.theme   | css/theme.css   |

  Scenario: Convert slash to dot in module name
    Given a JavaScript file with "@module paste/ui/heroscroll"
    When I extract the module name
    Then the module name should be "paste.ui.heroscroll"

  Scenario: Extract @requires dependencies
    Given a JavaScript file with content:
      """
      /**
       * @requires paste/dom
       * @requires paste/event
       * @module paste/ui/heroscroll
       */
      """
    When I extract the module metadata
    Then the dependencies should be:
      | dependency   |
      | paste.dom    |
      | paste.event  |

  Scenario: Fallback to filename when @module is missing
    Given a JavaScript file "src/js/utils.js" without @module comment
    When I build the asset manifest
    Then the manifest should NOT include the file in modules map
    And the file should still be in the assets map

  Scenario: Handle multiple modules in same build
    Given JavaScript files:
      | path                 | module_comment           |
      | src/js/site.js       | @module site/home         |
      | src/js/analytics.js  | @module site/analytics    |
    When I build the asset manifest
    Then the manifest modules should contain:
      | module_name    | file_path          |
      | site.home       | js/site.js         |
      | site.analytics  | js/analytics.js    |

  Scenario: Resolve module from manifest in combo handler
    Given a manifest with modules:
      | module_name | file_path      |
      | site.home    | js/site.js     |
    When the combo handler receives request for "site.home"
    Then it should load from "js/site.js"
    And NOT use filename convention fallback

  Scenario: Combo handler fallback when module not in manifest
    Given a manifest without "paste.dom" in modules map
    When the combo handler receives request for "paste.dom"
    Then it should use filename convention: "paste/src/js/dom.js"

  Scenario: Expand transitive dependencies at build time
    Given JavaScript files with dependencies:
      | path              | module      | requires           |
      | src/js/util.js    | paste.util  |                    |
      | src/js/dom.js     | paste.dom   | paste.util         |
      | src/js/event.js   | paste.event | paste.dom          |
    When I build the asset manifest
    Then module "paste.event" should have dependencies:
      | dependency  |
      | paste.dom   |
      | paste.util  |
    And module "paste.dom" should have dependencies:
      | dependency  |
      | paste.util  |

  Scenario: Generate sorted_deps in manifest
    Given JavaScript files with dependencies:
      | path              | module      | requires           |
      | src/js/util.js    | paste.util  |                    |
      | src/js/dom.js     | paste.dom   | paste.util         |
      | src/js/event.js   | paste.event | paste.dom          |
    When I build the asset manifest
    Then the manifest sorted_deps for "js" should be in order:
      | module      |
      | paste.util  |
      | paste.dom   |
      | paste.event |

  Scenario: Serve debug mode with raw source
    Given a JavaScript module "paste.dom" with source at "src/js/dom.js"
    When I request "/paste/123/paste.dom+v1.0.js"
    Then the response should contain the raw source content
    And NOT the minified version

  Scenario: Serve production mode with minified content
    Given a JavaScript module "paste.dom" with compiled output at "js/paste.dom.abc.v1.min.js"
    When I request "/paste/123/paste.dom+v1.0.min.js"
    Then the response should contain the minified content

  Scenario: SCSS compiled on-the-fly in debug mode
    Given an SCSS module "paste.ui.hero" at "modules/hero/hero.scss"
    And no compiled CSS exists
    When I request "/paste/123/paste.ui.hero+v1.0.css"
    Then the SCSS should be compiled to CSS
    And the response Content-Type should be "text/css"
