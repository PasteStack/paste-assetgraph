Feature: Build-Time Template Rendering
  As the paste-assetgraph build command
  I need to render JAM markup into HTML templates at build time
  So that a plain HTML site deploys to a static host with no request-time server

  Background:
    Given a manifest with compiled JS and CSS modules under the output directory
    And paste-surface.conf sets:
      | templates.source   | templates |
      | templates.output   | public    |

  Scenario: Directives render into pages under templates.output
    Given "templates/index.html" contains:
      """
      <!-- paste:jam css="site.theme" js="paste.dom" -->
      <!-- paste:dump css -->
      <!-- paste:dump js -->
      """
    When I run "paste-assetgraph build"
    Then "public/index.html" exists at the same relative path
    And the jam directive is replaced by nothing
    And the css dump is replaced by the stylesheet markup
    And the js dump is replaced by the runtime and loader markup
    And every other byte of the template is preserved verbatim

  Scenario: A bare dump emits both queues in one dump
    Given "templates/index.html" contains:
      """
      <!-- paste:jam css="site.theme" js="paste.dom" -->
      <!-- paste:dump -->
      """
    When I run "paste-assetgraph build"
    Then "public/index.html" exists at the same relative path
    And the single dump is replaced by the stylesheet markup followed by the runtime and loader markup
    # equivalent to "paste:dump css" then "paste:dump js", in one directive

  Scenario: Jam directive attributes
    Given a template line:
      """
      <!-- paste:jam css="a, b" js="c d" inline sync media="print" -->
      """
    Then the jam records css modules "a" and "b", js modules "c" and "d"
    And lists split on commas and whitespace
    And "inline" and "sync" take no value
    And "media" takes a quoted value

  Scenario: Malformed directives fail the build with file and line
    Given a template containing "paste:jam bogus", an unquoted attribute value, a valued "inline", or a "paste:dump" argument other than css or js
    When I run "paste-assetgraph build"
    Then the build fails naming the template file and line

  Scenario: Trailing jam after the last dump fails the build
    Given a template whose final directive is "paste:jam" with no dump after it
    When I run "paste-assetgraph build"
    Then the build fails — the jam would never be emitted

  Scenario: End of file finishes the page — a stranded css jam fails the build
    Given "templates/index.html" contains:
      """
      <!-- paste:dump css -->
      <!-- paste:jam css="site.theme" js="paste.dom" -->
      <!-- paste:dump js -->
      """
    When I run "paste-assetgraph build"
    Then the build fails naming the css module that was never dumped
    # css bound for the head must be jammed before the head's css dump;
    # finish detects the omission, it cannot move the tag back

  Scenario: A stranded js jam fails the build the same way
    Given "templates/index.html" contains:
      """
      <!-- paste:dump js -->
      <!-- paste:jam css="site.theme" js="paste.dom" -->
      <!-- paste:dump css -->
      """
    When I run "paste-assetgraph build"
    Then the build fails naming the js module that was never dumped

  Scenario: Css delivery is tracked per media attribute
    Given "templates/index.html" contains:
      """
      <!-- paste:jam css="site.theme" media="screen" -->
      <!-- paste:dump css -->
      <!-- paste:jam css="site.theme" media="print" -->
      <!-- paste:dump js -->
      """
    When I run "paste-assetgraph build"
    Then the build fails naming the module and its media attribute
    # the last directive is a dump, so the EOF finish — not the
    # trailing-jam guard — reports site.theme under media="print" as
    # a different tag that never shipped

  Scenario: The same module re-jammed under the same media is not outstanding
    Given "templates/index.html" contains:
      """
      <!-- paste:jam css="site.theme" media="screen" -->
      <!-- paste:dump css -->
      <!-- paste:jam css="site.theme" media="screen" -->
      <!-- paste:dump js -->
      """
    When I run "paste-assetgraph build"
    Then the build succeeds
    # the delivered (module, media) pair is not outstanding; the js jam
    # list is empty, so finish validates cleanly

  Scenario: Referenced combo files are materialized
    Given a template that renders external group URLs
    When I run "paste-assetgraph build"
    Then every URL the rendered page references exists as a file under output.base
    And a URL whose combo response is not 200 fails the build

  Scenario: Loader files are written for every JS module
    When I run "paste-assetgraph build"
    Then every manifest module with a JS asset has a file at "{output.base}/{base_path}/{packages-segment}/{module}.js"
    And the segment matches the packagesUrl the runtime renders

  Scenario: Non-HTML files and directive-free pages pass through
    Given "templates/app.js" and an HTML file with no paste directives
    When I run "paste-assetgraph build"
    Then both are copied to templates.output byte-identical

  Scenario: Output inside source is rejected
    Given "templates.output" equals "templates.source" or nests inside it
    When I run "paste-assetgraph build"
    Then the build fails before rendering — output cannot overwrite its source

  Scenario: jam.inline-threshold controls automatic inlining
    Given "jam.inline-threshold = 0" in paste-surface.conf
    When I run "paste-assetgraph build"
    Then rendered groups emit external tags, and their combo files are materialized
