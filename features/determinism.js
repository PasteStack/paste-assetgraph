/*jslint white:false plusplus:false browser:true nomen:false sub:true unparam:true*/
/*globals paste */

// @requires helper
paste.define("determinism", ["helper"], function (determinism, helper) {
    "use strict";
    return function () {
        return helper() === "helper";
    };
});
