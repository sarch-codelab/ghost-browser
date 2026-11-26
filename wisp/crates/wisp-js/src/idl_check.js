// Chequeo de conformidad entre los descriptores WebIDL de `wisp-idl` y el DOM
// real que `dom.js` acaba de instalar en el contexto.
//
// `__wispIdl` es un array de descriptores ({ name, attributes, operations })
// que prepara Rust. Recorremos cada interfaz contra `globalThis` y sus
// prototipos, y devolvemos un informe JSON con lo presente, lo ausente y los
// miembros que faltan. No modifica el runtime: solo lo interroga.
(function () {
    "use strict";
    const descriptors = __wispIdl || [];
    const presentInterfaces = [];
    const missingInterfaces = [];
    const missingMembers = [];
    let totalMembers = 0;
    let foundMembers = 0;
    const hasOwn = Object.prototype.hasOwnProperty;

    function hasMember(target, name) {
        let object = target;
        while (object) {
            if (hasOwn.call(object, name)) return true;
            object = Object.getPrototypeOf(object);
        }
        return false;
    }

    for (const descriptor of descriptors) {
        const ctor = globalThis[descriptor.name];
        const present = typeof ctor === "function";
        if (present) presentInterfaces.push(descriptor.name);
        else {
            // Una interfaz ausente ya se reporta como tal; no contamos sus
            // miembros para no duplicar el ruido.
            missingInterfaces.push(descriptor.name);
            continue;
        }

        const proto = ctor.prototype ? ctor.prototype : null;
        const members = []
            .concat(descriptor.attributes || [])
            .concat((descriptor.operations || []).map((name) => name + "()"));
        for (const member of members) {
            totalMembers++;
            const bare = member.slice(-2) === "()" ? member.slice(0, -2) : member;
            if (proto && hasMember(proto, bare)) foundMembers++;
            else missingMembers.push(descriptor.name + "." + member);
        }
    }

    return {
        presentInterfaces: presentInterfaces,
        missingInterfaces: missingInterfaces,
        missingMembers: missingMembers,
        totalMembers: totalMembers,
        foundMembers: foundMembers
    };
})()
