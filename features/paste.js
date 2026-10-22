/*!
 * Copyright (c) 2025–present Tomshley LLC
 * GitHub: https://github.com/sgoggles
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

/*jslint white:false plusplus:false browser:true nomen:false sub:true */
/*globals */
/**
 * The paste.js module is required for seeding and building paste, this includes the script loading mechanism.
 *
 *
 * @module paste
 */

(function (w, O) {
    'use strict';
    if (!O.keys) {
        /**
         * Hack for simple support of Object.keys.
         * Remove once we stop supporting <= IE9
         * Shamelessly inspired from: "http://tokenposts.blogspot.com.au/2012/04/javascript-objectkeys-browser.html"
         *
         * @param {Object} o
         * @return {Array}
         * @private
         */
        O.keys = function (o) {
            if (o !== O(o)) {
                throw new TypeError('Object.keys called on a non-object');
            }
            var k = [], p;
            for (p in o) {
                if (O.prototype.hasOwnProperty.call(o, p)) {
                    k.push(p);
                }
            }
            return k;
        };
    }

    /**
     * @class module:paste
     * @global
     * @return {module:paste}
     */
    var paste = w['__paste__'] || function () {
            // initialize the paste namespace
            var $paste = w['paste'] || this,
                instanceOf = function (obj, type) {
                    return (obj && obj.hasOwnProperty && (obj instanceof type));
                };

            if (!(instanceOf($paste, paste))) {
                $paste = new paste();
                $paste._instanceStart = new Date().getTime();
            }

            return $paste;
        },
        /**
         * we haven't loaded the dom module yet
         *
         * @name module:paste~_head
         * @type {HTMLElement}
         * @inner
         */
            _head = document.head || document.getElementsByTagName('head')[0],

        /**
         * ie < 9 has a stupid issue where the contents of the script will execute before it's actually "complete". this is lame and happens when the script is cached.
         *
         * @name module:paste~_useDirectAppend
         * @type {Boolean}
         * @inner
         */
            _useDirectAppend = !document.head,

        _isExternalScript = function (source) {
            return source.substr(source.lastIndexOf('.') + 1) === 'js' || source.lastIndexOf(':') > -1 || source.substr(0, 1) === '/';
        },
        /**
         * Module instances are stored in _modules. ModuleRelations are stored in each Module instance.
         *
         * @name module:paste~_modules
         * @type {Object}
         * @inner
         */
        _modules = {},
        _config = {};

    /**
     * @expose __paste__
     */
    w['__paste__'] = paste;

    /**
     * @expose paste
     */
    w['paste'] = paste();

    /*
     * configuration for the base url of packages passed to paste
     *
     * @name module:paste~_config
     * @inner
     * @property {string}  packagesUrl              - The default values for the packages url (e.g. '/js/').
     * @property {Function}  executionContext       - The default context that will be applied to modules.
     */
    _config = {
        'usePasteBuilder': true,
        'packagesUrl': '/paste/',
        'executionContext': w.paste
    };

    /**
     * @class module:paste~_ModuleRelation
     * @param {module:paste~_Module} moduleA
     * @param {module:paste~_Module} moduleB
     * @inner
     *
     * @property {module:paste~_Module#moduleA} moduleA
     * @property {module:paste~_Module#moduleB} moduleB
     * @property {module:paste~_Module#isSatisfied} isSatisfied
     */
    function _ModuleRelation (moduleA, moduleB) {
        this.moduleA = moduleA;
        this.moduleB = moduleB;


        this._key = null;
        this.isSatisfied = false;
    }
    _ModuleRelation.prototype = {
        /**
         * @function {module:paste~_Module#moduleA}
         * @return {String}
         */
        getKey: function () {
            if (!this._key) {
                this._key = this.moduleA.name + '|' + this.moduleB.name;
            }

            return this._key;
        },

        /**
         * @function {module:paste~_Module#trySatisfyActivate}
         * @return {Boolean}
         */
        trySatisfyActivate: function () {
            this.trySatisfy();

            if (this.isSatisfied) {
                this.moduleA.activate();
            }

//            console.log('trySatisfyActivate (' + this.isSatisfied + '): ' + this.getKey());
            return this.isSatisfied;
        },

        /**
         * @function {module:paste~_Module#trySatisfy}
         * @return {Boolean}
         */
        trySatisfy: function () {
//            console.log('trySatisfy (a:' + this.moduleA.name + ', b:' + this.moduleB.name + ')');

            if (this.moduleB.isDefined) {
                this.isSatisfied = this.moduleB.activate();
            } else {
                this.moduleB.load();
            }

            if (this.isSatisfied) {
                this.moduleA.updateRelation(this);
            }

//            console.log('trySatisfy (' + this.isSatisfied + '): ' + this.getKey());
            return this.isSatisfied;
        }
    };

    /**
     * @function module:paste~_ModuleRelation.add
     * @param {module:paste~_Module} moduleA
     * @param {module:paste~_Module} moduleB
     * @return {module:paste~_ModuleRelation}
     */
    _ModuleRelation.add = function (moduleA, moduleB) {
        var moduleRelation = new _ModuleRelation(moduleA, moduleB);
        moduleRelation.trySatisfy();
        return moduleRelation;
    };

    /**
     * @function module:paste~_ModuleRelation.getAll
     * @return {module:paste~_ModuleRelation[]}
     */
    _ModuleRelation.getAll = function () {
        var allModuleKeys = O.keys(_modules),
            i,
            ilength = allModuleKeys.length,
            allModuleRelations = [],
            moduleRelations = {},
            moduleRelationKeys = [],
            j = 0,
            jlength = 0;

        for (i = 0; i < ilength; i += 1) {
            moduleRelations = _modules[allModuleKeys[i]].moduleRelations;
            moduleRelationKeys = O.keys(moduleRelations);

            jlength = moduleRelationKeys.length;
            for (j = 0; j < jlength; j += 1) {
                allModuleRelations.push(moduleRelations[moduleRelationKeys[j]]);
            }
        }

        return allModuleRelations;
    };

    /**
     * A class to support the storage of a module name, dependencies, and a callback
     *
     * @class module:paste~_Module
     * @param name
     * @param dependencies
     * @param fn
     * @inner
     *
     * @property {module:paste~_Module#name} name
     * @property {module:paste~_Module#isDefined} isDefined
     * @property {module:paste~_Module#dependencies} dependencies
     * @property {module:paste~_Module#isLoading} isLoading
     * @property {module:paste~_Module#isLoaded} isLoaded
     * @property {module:paste~_Module#moduleRelations} moduleRelations
     */
    function _Module (name, dependencies, fn) {
        this.name = name;

        this._isExternal = _isExternalScript(name);

        this._isActive = false;

        /**
         * @type {Boolean}
         */
        this['isLoading'] = false;

        /**
         * @type {Boolean}
         */
        this['isLoaded'] = false;

        /**
         * @type {Object}
         */
        this.moduleRelations = {};

        /**
         * @type {Number}
         */
        this.errorCount = 0;

        /**
         * @type {Object}
         */
        this.errorTimeout = null;

        this.define(dependencies, fn);
    }
    _Module.prototype = {
        /**
         * @function module:paste~_Module#_parseDependencies
         * @private
         */
        _parseDependencies: function () {
            var dependencies = this.dependencies,
                i = 0,
                length = dependencies.length,
                relation;

            for (i; i < length; i += 1) {
                relation = _ModuleRelation.add(this, _Module.define(dependencies[i]));
                this.moduleRelations[relation.getKey()] = relation;
            }
        },

        /**
         * @function module:paste~_Module#_cleanup
         * @private
         */
        _cleanup: function () {
            delete this.moduleRelations;
            this.moduleRelations = {};

            delete this['isLoading'];
            delete this['isLoaded'];
            delete this.name;
            delete this.dependencies;
            delete this._fn;
            delete this._isExternal;

            delete this.errorCount;
            // this should be cleared as a result fo the "load()" call
            delete this.errorTimeout;
        },

        /**
         * @function module:paste~_Module#define
         * @param dependencies
         * @param fn
         * @return {Boolean}
         */
        define: function (dependencies, fn) {
            if (this._isActive) {
                return true;
            }

            if (this._isExternal) {
                dependencies = dependencies || [];
                this.load();
            }

            this.isDefined = typeof(dependencies) === 'array' || (dependencies !== null && typeof(dependencies) === 'object' && dependencies instanceof Array);
            if (!this.isDefined) {
                return false;
            }

            this.dependencies = dependencies;
            this._fn = fn;

            this._parseDependencies();

            this.activate();

            return true;
        },

        /**
         * @function module:paste~_Module#hasDirtyRelations
         * @param length
         * @return {Boolean}
         */
        hasDirtyRelations: function (length) {
            if (this._isActive) {
                return false;
            }

            length = length || O.keys(this.moduleRelations).length;
            return length > 0 || (this._isExternal && !this['isLoaded']);
        },

        /**
         * @function module:paste~_Module#updateRelation
         * @param {module:paste~_ModuleRelation} relation
         */
        updateRelation: function (relation) {
            if (this._isActive) {
                return;
            }

            if (relation.isSatisfied) {
                delete this.moduleRelations[relation.getKey()];
            }
        },
        /**
         * @function module:paste~_Module#activate
         * @return {Boolean} - Returns a boolean of whether the module has been activated
         */
        activate: function () {
            if (this._isActive || !this.isDefined) {
                return this._isActive;
            }

            var keys,
                i,
                length,
                relation,
                namespaceParts,
                activationParts,
                activationValue;

            if (this.hasDirtyRelations()) {
                keys = O.keys(this.moduleRelations);
                i = 0;
                length = keys.length;

                for (i; i < length; i += 1) {
                    relation = this.moduleRelations[keys[i]];
                    relation.trySatisfy();
                }

                if (this.hasDirtyRelations()) {
                    return false;
                }
            }

            this._isActive = true;

//            console.log('applying: ' + this.name);
            if (this._fn) {
                activationParts = _Module.getActivationParts(this.name, this.dependencies);
                activationValue = this._fn.apply(w['paste'], activationParts[1]);
                if (activationValue) {
                    namespaceParts = activationParts[0];
                    namespaceParts[0][namespaceParts[1]] = activationValue;
                }
            }

            // clean up properties to fee up memory
            this._cleanup();

            return this._isActive;
        },

        /**
         * @function module:paste~_Module#load
         */
        load: function () {
            if (this._isActive || this['isLoaded'] || this['isLoading']) {
                return;
            }

            window.clearTimeout(this.errorTimeout);
            this.errorTimeout = null;

//            console.log('loading: ' + this.name);
            var frag = document.createDocumentFragment(),
                sourceNode = document.createElement('script'),
                cachedNode;
            sourceNode.type = 'text/javascript';
            sourceNode.async = true;
            sourceNode.charset = 'utf-8';
            sourceNode.src = this._isExternal ? this.name : _Module.getURIFromName(this.name);
            sourceNode.data = this.name;

            if (sourceNode.addEventListener) {
                sourceNode.addEventListener('load', _Module.handleSourceSuccess, false);
                sourceNode.addEventListener('error', _Module.handleSourceError, false);
            } else if (sourceNode.attachEvent) {
                if (sourceNode.readyState) {
                    sourceNode.attachEvent('onreadystatechange', _Module.handleReadyStateChange);
                    sourceNode.attachEvent('onerror', _Module.handleSourceError);
                } else {
                    sourceNode.attachEvent('onload', _Module.handleSourceSuccess);
                    sourceNode.attachEvent('onerror', _Module.handleSourceError);
                }
            }

            if (_useDirectAppend) {
                cachedNode = sourceNode;
                _head.appendChild(sourceNode);
                cachedNode = null;
            } else {
                frag.appendChild(sourceNode);
                _head.appendChild(frag);
            }

            this['isLoading'] = true;
        }
    };
    /**
     * @function module:paste~_Module._disposeSourceListeners
     * @param sourceNode
     * @param module
     * @private
     */
    _Module._disposeSourceListeners = function (sourceNode, module) {
        module['isLoading'] = false;

        if (sourceNode.removeEventListener) {
            sourceNode.removeEventListener('load', _Module.handleSourceSuccess, false);
            sourceNode.removeEventListener('error', _Module.handleSourceError, false);
        } else if (sourceNode.detachEvent) {
            if (sourceNode.readyState) {
                sourceNode.detachEvent('onreadystatechange', _Module.handleReadyStateChange);
                sourceNode.detachEvent('onerror', _Module.handleSourceError);
            } else {
                sourceNode.detachEvent('onload', _Module.handleSourceSuccess);
                sourceNode.detachEvent('onerror', _Module.handleSourceError);
            }
        }
    };

    /**
     * @function module:paste~_Module.handleReadyStateChange
     * @param {Event} e
     */
    _Module.handleReadyStateChange = function (e) {
        // todo - log when sourcenode is null in IE
        var sourceNode = e.target || e.srcElement,
            valid = sourceNode && (sourceNode.readyState.toLowerCase() === 'loaded' || sourceNode.readyState.toLowerCase() === 'complete');
        if (valid) {
            _Module.handleSourceSuccess(e);
        }
    };

    /**
     * @function module:paste~_Module.handleSourceSuccess
     * @param {Event} e
     */
    _Module.handleSourceSuccess = function (e) {
        var sourceNode = e.target || e.srcElement,
            moduleRelations = _ModuleRelation.getAll(),
            length = moduleRelations.length,
            i = 0,
            moduleName = sourceNode.data,
            module = _modules[moduleName];

        _Module._disposeSourceListeners(sourceNode, module);
        module['isLoaded'] = true;

        // fixme: make this more efficient - garbage collection? something else?

        for (i; i < length; i += 1) {
            moduleRelations[i].trySatisfyActivate();
        }
    };

    /**
     * @function module:paste~_Module.handleSourceError
     * @param {Event} e
     */
    _Module.handleSourceError = function (e) {
        var sourceNode = e.target || e.srcElement,
            sourceNodeSource = sourceNode ? sourceNode.src : null,
            moduleName = sourceNode.data,
            module = _modules[moduleName];

        // not the cleanest, but otherwise remove event listeners doesn't work well and trying to stay away from closure
        _Module._disposeSourceListeners(sourceNode, module);

        _head.removeChild(sourceNode);

        module.errorCount += 1;
        window.clearTimeout(module.errorTimeout);
        if (module.errorCount <= 5) {
            module.errorTimeout = window.setTimeout(function () {
                module.load();
            }, 5000);
        } else {
            throw new Error('Error loading resource. Max attempts reached. ' + sourceNodeSource);
        }

        sourceNodeSource = null;
    };

    /**
     * @function module:paste~_Module.getURIFromName
     * @param {String} moduleName
     * @return {String} - uri of the module
     */
    _Module.getURIFromName = function (moduleName) {
        if (moduleName.length < 1) {
            throw new Error('paste.require - please specify a correct module name and package hierarchy (empty string)');
        }

        if (moduleName.lastIndexOf('/') > -1) {
            throw new Error('paste.require - please specify a correct module name and package hierarchy');
        }

        // exposing here for now - for performance, we may want to look at doing this in each module
        return _config['packagesUrl'] + (_config['usePasteBuilder'] ? moduleName : moduleName.replace('.', '/')) + '.js';
    };

    /**
     * @function module:paste~_Module.getActivationParts
     * @param {String} moduleName
     * @param {String[]} dependencies
     * @return {Object[]}
     */
    _Module.getActivationParts = function (moduleName, dependencies) {
        var getNamespaceObj = function (name) {
                var part = name.split('.'),
                    i = 0,
                    length = part.length,
                    namespace = window,
                    namespaceParent,
                    partName;

                for (i; i < length; i += 1) {
                    partName = part[i];
                    namespaceParent = namespace;
                    namespace[partName] = namespace[partName] || {};
                    namespace = namespace[partName];
                }

                return [namespaceParent, partName, namespace];
            },
            namespaceParts = getNamespaceObj(moduleName),
            args = [namespaceParts[2]],
            i = 0,
            length = dependencies.length;

        for (i; i < length; i += 1) {
            args.push(getNamespaceObj(dependencies[i])[2]);
        }

        return [namespaceParts, args];
    };

    /**
     * @function module:paste~_Module.define
     * @param moduleName
     * @param [dependencies]
     * @param fn
     * @return {module:paste~_Module}
     */
    _Module.define = function (moduleName, dependencies, fn) {
        if (typeof moduleName !== 'string') {
            fn = dependencies;
            dependencies = moduleName;
            // potential fixme: we may need something more "unique" here
            moduleName = 'paste._internal$' + new Date().getTime() + (((1 + Math.random()) * 0x10000) | 0).toString(16).substring(1);
        }

        // allow optional dependency param
        if ((typeof(dependencies) === 'function' || (dependencies !== null && typeof(dependencies) === 'object' && dependencies instanceof Function))) {
            fn = dependencies;
            dependencies = [];
        }

        if (!_modules.hasOwnProperty(moduleName)) {
            _modules[moduleName] = new _Module(moduleName, dependencies, fn);
        } else if (!_modules[moduleName].isDefined) {
            _modules[moduleName].define(dependencies, fn);
        }

        return _modules[moduleName];
    };

    /**
     * @function module:paste.define
     * @see module:paste~_Module.define
     */
    w['paste']['define'] = _Module.define;
    w['paste']['define']['amd'] = {
        'multiversion': false
    };

    /**
     * @function module:paste.require
     * @param [dependencies]
     * @param fn
     */
    w['paste']['require'] = function (dependencies, fn) {
        if (typeof(dependencies) === 'string' || (dependencies !== null && typeof(dependencies) === 'object' && dependencies instanceof String)) {
            dependencies = [dependencies];
        }

        return _Module.define(dependencies, fn);
    };


    /**
     *
     * @function module:paste.config
     * @param {Object} config
     *
     */
    w['paste']['config'] = function (config) {
        // rudimentary (non-existent really) object mixin
        if (config.hasOwnProperty('packagesUrl')) {
            _config['packagesUrl'] = config['packagesUrl'];
        }
    };

    /**
     *
     * @property module:paste.instanceStarted
     *
     */
    w['paste']['instanceStart'] = w['paste']._instanceStart;
}(window, Object));
