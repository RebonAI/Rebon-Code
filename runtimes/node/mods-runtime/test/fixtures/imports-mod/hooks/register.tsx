import type { Register } from 'claude-code';
import { card } from './card';
import { shout } from './util.js';
import { fromIndex } from './lib';

export const register: Register = (on) => {
  on('session.start', async ($, e, next) => {
    $.ui.status(card(shout(fromIndex)));
    return next(e);
  });
};
