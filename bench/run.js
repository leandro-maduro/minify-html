const fs = require('fs');
const mkdirp = require('mkdirp');
const path = require('path');
const minifiers = require('./minifiers');
const results = require('./results');
const tests = require('./tests');

const sizes = {};
const setSize = (program, test, result) => {
  if (!sizes[test]) {
    sizes[test] = {
      original: {
        absolute: tests.find(t => t.name === test).contentAsBuffer.length,
        relative: 1,
      },
    };
  }
  const original = sizes[test].original.absolute;
  sizes[test][program] = {
    absolute: result,
    relative: result / original,
  };
};

for (const t of tests) {
  for (const m of Object.keys(minifiers)) {
    try {
      const min = minifiers[m](t.contentAsString, t.contentAsBuffer);
      setSize(m, t.name, min.length);
      const minPath = path.join(__dirname, 'min', m, `${t.name}.html`);
      mkdirp.sync(path.dirname(minPath));
      fs.writeFileSync(minPath, min);
    } catch (err) {
      console.error(`Failed to run ${m} on test ${t.name}:`);
      console.error(err);
      process.exit(1);
    }
  }
}
results.writeSizeResults(sizes);
