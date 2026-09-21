// Isolate probe provider: a cordis Service providing `probe` with a
// config-chosen value. Loaded twice — once globally, once inside an
// isolated group — to prove in-group/out-group coexistence.
import { Service } from 'cordis';

export default class ProbeService extends Service {
  constructor(ctx, config) {
    super(ctx, 'probe');
    // `_` prefix, not #private: Service instances live behind a Proxy.
    this._value = config?.value ?? 'unset';
  }

  value() {
    return this._value;
  }
}
