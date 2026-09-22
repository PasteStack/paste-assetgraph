resolvers +=
  "gitlab-maven" at "https://gitlab.com/api/v4/projects/tomshley%2Fbrands%2Fglobal%2Ftware%2Ftech%2Fproducts%2Fmagicroot%2Fmagicroot-registry/packages/maven"

addSbtPlugin(
  "com.tomshley.magicroot" % "magicroot-sbt-projectsettings" % "2.2.0"
)
