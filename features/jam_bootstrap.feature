Feature: JAM Bootstrap Rendering
  As the paste-assetgraph render command
  I need to turn a page's jam/dump event list into markup
  So that every runtime and the build-time renderer emit identical tags

  Background:
    Given a manifest with a compiled "paste" runtime module
    And a manifest with modules:
      | module        | asset key         | dependencies |
      | paste         | js/paste.js       |              |
      | paste.dom     | js/paste/dom.js   | paste        |
      | paste.event   | js/paste/event.js | paste.dom    |
      | site.theme    | css/site/theme.css|              |
    And compiled output for every asset under the output directory

  Scenario: First js dump inlines the runtime with the packages config
    When I render the events:
      | {"jam":{"js":["paste.dom"]}} |
      | {"dump":"js"}               |
    Then the first tag is an inline script containing the runtime source
    And it contains "paste.config({packagesUrl:'/paste/{segment}/'})"
    And {segment} is derived from the manifest's JS asset content

  Scenario: Packages segment follows JS content
    Given two manifests differing only in one JS asset's hash
    Then their rendered packagesUrl segments differ
    And identical manifests render identical segments

  Scenario: Runtime emitted exactly once per page
    When I render the events:
      | {"jam":{"js":["paste.dom"]}}   |
      | {"dump":"js"}                  |
      | {"jam":{"js":["paste.event"]}} |
      | {"dump":"js"}                  |
    Then the runtime script appears exactly once, before the first group

  Scenario: Async loader marks pending modules
    When I render a jam for js modules "paste.dom" and "paste.event" with dump js
    And the group is over the inline threshold
    Then the loader script contains "d('paste.dom').isLoading" and "d('paste.event').isLoading"
    And it requires the exact-group URL via "r('{url}')"

  Scenario: Group URL names the exact filtered list
    When I render a jam for js modules "paste.dom" and "paste.event" with dump js
    And the group is over the inline threshold
    Then the URL is "/paste/{ts}/paste.dom+vN.0~paste.event+vN.0.min.js"
    And "paste" is not in the URL — it was delivered by the runtime
    And {ts} is the maximum timestamp over the delivered modules

  Scenario: Modules already delivered are dropped from later groups
    When I render the events:
      | {"jam":{"js":["paste.dom"]}}   |
      | {"dump":"js"}                  |
      | {"jam":{"js":["paste.event"]}} |
      | {"dump":"js"}                  |
    Then the second group's URL names only "paste.event"
    And it carries no query parameter for the delivered modules

  Scenario: Modules ordered by dependency, not request order
    When I render a jam for js modules "paste.event" then "paste.dom" with dump js
    Then the URL lists "paste.dom" before "paste.event"

  Scenario: Sync form emits a script source tag
    When I render a jam for js module "paste.dom" with sync=true and dump js
    Then the tag is "<script type="text/javascript" src="/paste/{ts}/paste.dom+vN.0.min.js"></script>"

  Scenario: Group under the inline threshold is inlined
    Given inline-threshold 1024 and a group below it
    When I render a jam and dump
    Then the group emits "<script type="text/javascript">…compiled content…</script>"

  Scenario: Inline threshold zero disables automatic inlining
    Given inline-threshold 0
    When I render a jam and dump
    Then the group emits the external loader tag, never inline content

  Scenario: Inline flag forces inlining regardless of threshold
    Given inline-threshold 0
    When I render a jam with inline=true and dump
    Then the group emits an inline script with the compiled content

  Scenario: Unsafe JavaScript never inlines
    Given a module whose compiled output contains "</script" or "<!--"
    When I render a jam with inline=true and dump js
    Then the group emits the external loader tag, not an inline script

  Scenario: Unsafe CSS never inlines
    Given a module whose compiled output contains "</style"
    When I render a jam with inline=true and dump css
    Then the group emits a stylesheet link, not an inline style

  Scenario: CSS emits a stylesheet link or inline style
    When I render a jam for css module "site.theme" with dump css
    Then a large group emits "<link rel="stylesheet" href="/paste/{ts}/site.theme+vN.0.min.css"/>"
    And a small group emits an inline "<style type="text/css">…</style>"

  Scenario: CSS media attribute
    When I render a jam for css module "site.theme" with media="print" and dump css
    Then the link tag carries media="print"

  Scenario: A jam naming an unknown module fails the render
    When I render the events:
      | {"jam":{"js":["nonexistent.mod"]}} |
      | {"dump":"js"}                    |
    Then the render fails with an error naming "nonexistent.mod" and "js"

  Scenario: A jam naming a module without a matching-type asset fails
    When I render the events:
      | {"jam":{"css":["paste.dom"]}} |
      | {"dump":"css"}              |
    Then the render fails with an error naming "paste.dom" and "css"

  Scenario: A js jam on a manifest without the paste runtime fails
    Given a manifest with no "paste" js asset
    When I render the events:
      | {"jam":{"js":["paste.dom"]}} |
      | {"dump":"js"}              |
    Then the render fails with an error naming "paste" and "js"

  Scenario: Each dump renders the full event list
    When I render the events:
      | {"jam":{"css":["site.theme"],"js":["paste.dom"]}} |
      | {"dump":"css"}                                    |
      | {"dump":"js"}                                     |
    Then the css dump emits the stylesheet markup
    And the js dump emits the runtime and the js group markup
    And modules delivered by the css dump do not change the js group
