/* MONITOR — camera viewfinder logic
 * 纯前端：画面只在浏览器内处理，不上传任何数据。
 */
(function () {
  'use strict';

  var $ = function (id) { return document.getElementById(id); };
  var el = {
    video: $('video'), remoteVideo: $('remoteVideo'), stage: $('stage'), idle: $('idle'), hud: $('hud'),
    startBtn: $('startBtn'), hint: $('hint'), shutterLabel: document.querySelector('.shutter-label'),
    castInvite: $('castInvite'), castDismiss: $('castDismiss'), castStartBtn: $('castStartBtn'),
    localCamBtn: $('localCamBtn'),
    stageWait: $('stageWait'), waitQr: $('waitQr'), waitUrl: $('waitUrl'), waitStatus: $('waitStatus'), waitHint: $('waitHint'),
    phoneCastLabel: $('phoneCastLabel'),
    phoneCastBtn: $('phoneCastBtn'), rtcWait: $('rtcWait'),
    rtcStatus: $('rtcStatus'), rtcNote: $('rtcNote'), rtcQuality: $('rtcQuality'),
    rtcCopy: $('rtcCopy'), rtcCancel: $('rtcCancel'),
    castBar: $('castBar'), castText: $('castText'), castStop: $('castStop'),
    statusPill: $('statusPill'), statusText: $('statusText'),
    resPill: $('resPill'), fpsPill: $('fpsPill'), clock: $('clock'),
    hudFacing: $('hudFacing'), hudRes: $('hudRes'), hudTime: $('hudTime'),
    hudRec: $('hudRec'), recTime: $('recTime'),
    captureBtn: $('captureBtn'), recordBtn: $('recordBtn'), recordLabel: $('recordLabel'),
    switchBtn: $('switchBtn'), mirrorBtn: $('mirrorBtn'), gridBtn: $('gridBtn'),
    torchBtn: $('torchBtn'), fullBtn: $('fullBtn'),
    grid: $('grid'), flash: $('flash'),
    filmstrip: $('filmstrip'), shotCount: $('shotCount'), clearBtn: $('clearBtn'),
    metaDevice: $('metaDevice'), metaRes: $('metaRes'), metaSecure: $('metaSecure'), metaHost: $('metaHost'),
    qrPanel: $('qrPanel'), qrBox: $('qrBox'), qrUrl: $('qrUrl'), qrNote: $('qrNote'),
    toast: $('toast'), lightbox: $('lightbox'), lbStage: $('lbStage'),
    lbMeta: $('lbMeta'), lbDownload: $('lbDownload'), lbDelete: $('lbDelete'),
    lbClose: $('lbClose')
  };

  var state = {
    stream: null,
    recorder: null,
    chunks: [],
    recording: false,
    recStarted: 0,
    recTimer: null,
    shots: [],
    activeShot: null,
    mirror: false,
    grid: true,
    torchOn: false,
    deviceIndex: -1,
    facing: 'environment',
    videoDevices: [],
    fps: 0,
    started: false,
    remote: false,
    remoteStream: null
  };

  var isSecure = window.isSecureContext && !!navigator.mediaDevices && !!navigator.mediaDevices.getUserMedia;

  /* 保持屏幕常亮（监控场景很实用） */
  var wakeLock = null;
  function keepAwake() {
    if (!('wakeLock' in navigator) || document.visibilityState !== 'visible') return;
    navigator.wakeLock.request('screen').then(function (lock) {
      wakeLock = lock;
      lock.addEventListener('release', function () { wakeLock = null; });
    }).catch(function () { /* 不支持或被拒绝时静默忽略 */ });
  }
  function releaseWake() {
    if (wakeLock) { wakeLock.release().catch(function () {}); wakeLock = null; }
  }
  document.addEventListener('visibilitychange', function () {
    if (document.visibilityState === 'visible' && state.started) keepAwake();
  });

  /* ---------------- helpers ---------------- */
  var toastTimer = null;
  function toast(message, isError) {
    el.toast.textContent = message;
    el.toast.classList.toggle('is-error', !!isError);
    el.toast.classList.add('is-on');
    clearTimeout(toastTimer);
    toastTimer = setTimeout(function () { el.toast.classList.remove('is-on'); }, 2600);
  }

  function setStatus(kind, text) {
    el.statusPill.dataset.state = kind;
    el.statusText.textContent = text;
  }

  function pad(n) { return String(n).padStart(2, '0'); }

  function stamp(d) {
    d = d || new Date();
    return d.getFullYear() + pad(d.getMonth() + 1) + pad(d.getDate()) + '-' +
      pad(d.getHours()) + pad(d.getMinutes()) + pad(d.getSeconds());
  }

  function clockText(d) {
    d = d || new Date();
    return pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds());
  }

  function fmtBytes(b) {
    if (b < 1024) return b + ' B';
    if (b < 1024 * 1024) return Math.round(b / 1024) + ' KB';
    return (b / 1024 / 1024).toFixed(1) + ' MB';
  }

  function fmtDuration(ms) {
    var s = Math.floor(ms / 1000);
    return pad(Math.floor(s / 60)) + ':' + pad(s % 60);
  }

  /* ---------------- clock ---------------- */
  setInterval(function () {
    var t = clockText();
    el.clock.textContent = t;
    el.hudTime.textContent = t;
  }, 1000);

  /* ---------------- camera ---------------- */
  function describeError(err) {
    var name = err && err.name;
    if (name === 'NotAllowedError' || name === 'SecurityError')
      return '摄像头权限被拒绝。请在浏览器地址栏的权限设置里允许摄像头，然后重试。';
    if (name === 'NotFoundError' || name === 'OverconstrainedError')
      return '没有找到可用的摄像头设备。';
    if (name === 'NotReadableError' || name === 'AbortError')
      return '摄像头正被其他应用占用（例如相机、视频会议软件），请关闭后重试。';
    if (!window.isSecureContext)
      return '当前页面不是安全来源，浏览器禁止访问摄像头。请通过 https:// 或 localhost 打开。';
    return '无法启动摄像头：' + (err && err.message ? err.message : String(err));
  }

  function listCameras() {
    return navigator.mediaDevices.enumerateDevices().then(function (devices) {
      state.videoDevices = devices.filter(function (d) { return d.kind === 'videoinput'; });
      return state.videoDevices;
    }).catch(function () {
      state.videoDevices = [];
      return state.videoDevices;
    });
  }

  /* 越高越好：手机会给到它支持的最高档（通常是 1920×1080）。
     模糊多半是这里被降到 640×480 或编码砍了分辨率，不是显示层的问题。 */
  function pickConstraints(deviceId) {
    var video = {
      width: { ideal: 1920 }, height: { ideal: 1080 },
      frameRate: { ideal: 30, max: 30 }
    };
    if (deviceId) {
      video.deviceId = { exact: deviceId };
    } else if (state.facing === 'user') {
      video.facingMode = 'user';
    } else if (state.facing === 'environment') {
      video.facingMode = { ideal: 'environment' };
    }
    return { audio: false, video: video };
  }

  function startCamera(deviceId) {
    if (!isSecure) {
      setStatus('error', '不安全来源');
      el.hint.textContent = location.protocol === 'file:'
        ? '直接双击打开时浏览器会拦截摄像头 — 请运行 python3 serve.py 后访问 localhost'
        : '需要 HTTPS 或 localhost 才能访问摄像头';
      el.hint.classList.add('is-warn');
      toast(describeError(new Error('insecure')), true);
      return Promise.resolve();
    }
    el.startBtn.disabled = true;
    setStatus('idle', '连接中');
    if (state.stream) state.stream.getTracks().forEach(function (t) { t.stop(); });

    return navigator.mediaDevices.getUserMedia(pickConstraints(deviceId))
      .catch(function (err) {
        // 部分设备不支持 ideal 约束，退回到最宽松的请求
        if (err && (err.name === 'OverconstrainedError' || err.name === 'NotFoundError')) {
          return navigator.mediaDevices.getUserMedia({ audio: false, video: true });
        }
        throw err;
      })
      .then(function (stream) {
        state.stream = stream;
        var track = stream.getVideoTracks()[0];
        el.video.srcObject = stream;
        // 注意：不要 await play()。元素在后台/首帧未就绪时这个 Promise 可能长时间不结算，
        // 会把后面的「建立 WebRTC 连接」一起卡住。
        var played = el.video.play().catch(function () {});
        state.started = true;

        var settings = track.getSettings ? track.getSettings() : {};
        if (settings.deviceId) {
          state.deviceIndex = Math.max(0, state.videoDevices.findIndex(function (d) { return d.deviceId === settings.deviceId; }));
        }
        if (settings.facingMode) state.facing = settings.facingMode;

        var label = track.label || '摄像头';
        state.mirror = /front|user|facetime|内建|前置/i.test(label) || state.facing === 'user';

        el.stage.classList.add('is-live');
        el.stage.classList.toggle('mirrored', state.mirror);
        el.grid.classList.toggle('is-on', state.grid);
        el.mirrorBtn.setAttribute('aria-pressed', String(state.mirror));
        el.mirrorBtn.disabled = false;
        el.gridBtn.disabled = false;
        el.captureBtn.disabled = false;
        el.recordBtn.disabled = false;
        el.fullBtn.disabled = false;
        el.switchBtn.disabled = false;
        el.hint.classList.remove('is-warn');
        keepAwake();

        return listCameras().then(function (devices) {
          el.switchBtn.disabled = devices.length < 2;
          var caps = track.getCapabilities ? track.getCapabilities() : {};
          el.torchBtn.hidden = !caps.torch;
          el.torchBtn.setAttribute('aria-pressed', 'false');
          state.torchOn = false;

          el.metaDevice.textContent = label.length > 26 ? label.slice(0, 26) + '…' : label;
          el.hudFacing.textContent = state.facing === 'user' ? '前置' : (state.facing === 'environment' ? '后置' : '默认');
          setStatus('live', '实时画面');
          updateResolution();
          startFpsMeter();
          toast('摄像头已开启');

          track.addEventListener('ended', function () {
            setStatus('idle', '已断开');
            el.stage.classList.remove('is-live');
            toast('摄像头连接已断开', true);
          });
          return Promise.race([played, new Promise(function (r) { setTimeout(r, 400); })]);
        });
      })
      .catch(function (err) {
        console.error(err);
        setStatus('error', '不可用');
        var msg = describeError(err);
        el.hint.textContent = msg;
        el.hint.classList.add('is-warn');
        toast(msg, true);
        el.startBtn.disabled = false;
      });
  }

  function activeVideo() {
    return state.remote && el.remoteVideo.videoWidth ? el.remoteVideo : el.video;
  }

  function updateResolution() {
    var src = activeVideo();
    var w = src.videoWidth, h = src.videoHeight;
    if (!w || !h) return;
    var text = w + ' × ' + h;
    el.resPill.textContent = text;
    el.hudRes.textContent = text;
    el.metaRes.textContent = text;
    // 竖屏来源时把舞台也变成竖的：contain 在 16:10 的框里会把竖画面压得很小
    el.stage.classList.toggle('is-portrait', h > w * 1.02);
  }
  el.video.addEventListener('loadedmetadata', updateResolution);
  el.video.addEventListener('resize', updateResolution);
  el.remoteVideo.addEventListener('loadedmetadata', updateResolution);
  el.remoteVideo.addEventListener('resize', updateResolution);

  /* FPS 计数 */
  var fpsToken = 0;
  /* 本地 / 手机画面切换时旧的计数链会断（源没帧了），用 token 让新链接管 */
  function startFpsMeter() {
    if (!('requestVideoFrameCallback' in HTMLVideoElement.prototype)) return;
    var mine = ++fpsToken;
    var count = 0, t0 = performance.now();
    var tick = function () {
      if (mine !== fpsToken) return;
      // 远端画面刚接上时 videoWidth 可能还是 0，这里以 state.remote 为准
      var src = state.remote ? el.remoteVideo : el.video;
      count++;
      var now = performance.now();
      if (now - t0 >= 1000) {
        state.fps = Math.round((count * 1000) / (now - t0));
        el.fpsPill.textContent = state.fps + ' FPS';
        count = 0; t0 = now;
      }
      src.requestVideoFrameCallback(tick);
    };
    (state.remote ? el.remoteVideo : el.video).requestVideoFrameCallback(tick);
  }
  startFpsMeter();

  /* ---------------- 画质诊断 ---------------- */
  /* 手机推流时把实际送到电脑的分辨率/码率显示出来：糊的时候能一眼看出
     是采集端就没给高清，还是编码把分辨率砍了。 */
  var QUALITY_REASONS = {
    none: '未受限',
    bandwidth: '网络带宽不足',
    cpu: '手机编码性能不足',
    other: '其它原因'
  };
  var qualityTimer = null;

  function setQuality(text, warn) {
    if (!el.rtcQuality) return;
    if (!text) { el.rtcQuality.hidden = true; el.rtcQuality.textContent = ''; return; }
    el.rtcQuality.hidden = false;
    el.rtcQuality.textContent = text;
    el.rtcQuality.classList.toggle('is-warn', !!warn);
  }

  function sampleQuality() {
    clearTimeout(qualityTimer);
    qualityTimer = null;
    if (!state.remote || !rtc.pc || !rtc.pc.getStats) { setQuality(''); return; }
    rtc.pc.getStats().then(function (report) {
      if (!state.remote) return;
      var inbound = null;
      report.forEach(function (r) {
        if (r.type === 'inbound-rtp' && r.kind === 'video') inbound = r;
      });
      if (!inbound || !inbound.frameWidth) {
        setQuality('正在接收画面…');
      } else {
        var low = inbound.frameWidth < 1280;
        var reason = QUALITY_REASONS[inbound.qualityLimitationReason] || inbound.qualityLimitationReason || '';
        var text = inbound.frameWidth + '×' + inbound.frameHeight + ' · ' + (inbound.framesPerSecond || 0) + ' FPS';
        if (low) text += ' · 偏低';
        if (reason && inbound.qualityLimitationReason !== 'none') text += ' · ' + reason;
        setQuality(text, low || (inbound.qualityLimitationReason && inbound.qualityLimitationReason !== 'none'));
      }
      qualityTimer = setTimeout(sampleQuality, 3000);
    }).catch(function () { setQuality(''); });
  }

  /* ---------------- capture ---------------- */
  function drawFrame() {
    var src = activeVideo();
    var w = src.videoWidth, h = src.videoHeight;
    var canvas = document.createElement('canvas');
    canvas.width = w; canvas.height = h;
    var ctx = canvas.getContext('2d');
    if (state.mirror && src === el.video) {
      ctx.translate(w, 0);
      ctx.scale(-1, 1);
    }
    ctx.drawImage(src, 0, 0, w, h);
    return canvas;
  }

  function capture() {
    var src = activeVideo();
    if (!src || !src.videoWidth) {
      toast('摄像头还没有准备好', true);
      return;
    }
    var canvas = drawFrame();
    canvas.toBlob(function (blob) {
      if (!blob) { toast('拍照失败，请重试', true); return; }
      addShot({
        kind: 'photo',
        blob: blob,
        url: URL.createObjectURL(blob),
        width: canvas.width,
        height: canvas.height,
        duration: 0,
        name: 'monitor-' + stamp() + '.jpg'
      });
      flash();
      toast('已拍摄 · ' + fmtBytes(blob.size));
    }, 'image/jpeg', 0.92);
  }

  function flash() {
    el.flash.classList.remove('is-firing');
    void el.flash.offsetWidth;
    el.flash.classList.add('is-firing');
    if (navigator.vibrate) navigator.vibrate(12);
  }

  /* ---------------- recording ---------------- */
  function pickMimeType() {
    if (!window.MediaRecorder) return '';
    var candidates = [
      'video/mp4;codecs=avc1.42E01E',
      'video/mp4',
      'video/webm;codecs=vp9',
      'video/webm;codecs=vp8',
      'video/webm'
    ];
    for (var i = 0; i < candidates.length; i++) {
      try { if (MediaRecorder.isTypeSupported(candidates[i])) return candidates[i]; } catch (e) {}
    }
    return '';
  }

  function recordStream() {
    return state.remote && state.remoteStream ? state.remoteStream : state.stream;
  }

  function startRecording() {
    var stream = recordStream();
    if (!stream || state.recording) return;
    if (!window.MediaRecorder) { toast('当前浏览器不支持录制视频', true); return; }
    var mimeType = pickMimeType();
    var recorder;
    try {
      recorder = new MediaRecorder(stream, mimeType ? { mimeType: mimeType, videoBitsPerSecond: 6000000 } : undefined);
    } catch (err) {
      console.error(err);
      toast('无法开始录制：' + err.message, true);
      return;
    }
    state.chunks = [];
    recorder.ondataavailable = function (e) { if (e.data && e.data.size) state.chunks.push(e.data); };
    recorder.onstop = function () {
      var type = recorder.mimeType || mimeType || 'video/webm';
      var blob = new Blob(state.chunks, { type: type });
      state.chunks = [];
      if (!blob.size) { toast('录制没有产生数据', true); return; }
      var ext = type.indexOf('mp4') >= 0 ? 'mp4' : 'webm';
      addShot({
        kind: 'video',
        blob: blob,
        url: URL.createObjectURL(blob),
        width: activeVideo().videoWidth,
        height: activeVideo().videoHeight,
        duration: Date.now() - state.recStarted,
        name: 'monitor-' + stamp() + '.' + ext
      });
      toast('录像已保存 · ' + fmtBytes(blob.size));
    };
    state.recorder = recorder;
    state.recStarted = Date.now();
    recorder.start(1000);
    state.recording = true;

    el.recordBtn.classList.add('is-on');
    el.recordLabel.textContent = '停止录制';
    el.hudRec.classList.add('is-on');
    el.recTime.textContent = '00:00';
    setStatus('rec', '录制中');
    state.recTimer = setInterval(function () {
      el.recTime.textContent = fmtDuration(Date.now() - state.recStarted);
    }, 250);
  }

  function stopRecording() {
    if (!state.recording || !state.recorder) return;
    state.recording = false;
    try { state.recorder.stop(); } catch (e) {}
    clearInterval(state.recTimer);
    state.recTimer = null;
    el.recordBtn.classList.remove('is-on');
    el.recordLabel.textContent = '开始录制';
    el.hudRec.classList.remove('is-on');
    setStatus('live', '实时画面');
  }

  function toggleRecording() {
    if (state.recording) stopRecording(); else startRecording();
  }

  /* ---------------- shots ---------------- */
  function addShot(shot) {
    shot.id = 'shot-' + Math.random().toString(36).slice(2, 9);
    state.shots.unshift(shot);
    renderShots();
    el.filmstrip.scrollLeft = 0;
  }

  function renderShots() {
    el.shotCount.textContent = String(state.shots.length);
    el.filmstrip.innerHTML = '';
    if (!state.shots.length) {
      var p = document.createElement('p');
      p.className = 'empty';
      p.textContent = '还没有照片 — 按下快门开始';
      el.filmstrip.appendChild(p);
      return;
    }
    state.shots.forEach(function (shot) {
      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'shot' + (shot.kind === 'video' ? ' is-video' : '');
      btn.dataset.id = shot.id;
      btn.setAttribute('aria-label', (shot.kind === 'video' ? '录像 ' : '照片 ') + shot.name);

      var media = document.createElement(shot.kind === 'video' ? 'video' : 'img');
      media.src = shot.url;
      if (shot.kind === 'video') {
        media.muted = true;
        media.playsInline = true;
        media.preload = 'metadata';
      } else {
        media.alt = '';
        media.loading = 'lazy';
      }
      btn.appendChild(media);

      var tag = document.createElement('span');
      tag.className = 'tag';
      tag.textContent = shot.kind === 'video' ? fmtDuration(shot.duration) : fmtBytes(shot.blob.size);
      btn.appendChild(tag);

      var kind = document.createElement('span');
      kind.className = 'kind';
      kind.textContent = shot.kind === 'video' ? '▶' : '◉';
      btn.appendChild(kind);

      btn.addEventListener('click', function () { openLightbox(shot.id); });
      el.filmstrip.appendChild(btn);
    });
  }

  function openLightbox(id) {
    var shot = null;
    for (var i = 0; i < state.shots.length; i++) {
      if (state.shots[i].id === id) { shot = state.shots[i]; break; }
    }
    if (!shot) return;
    state.activeShot = shot;
    el.lbStage.innerHTML = '';
    if (shot.kind === 'video') {
      var v = document.createElement('video');
      v.src = shot.url;
      v.controls = true;
      v.autoplay = true;
      v.playsInline = true;
      v.loop = true;
      el.lbStage.appendChild(v);
    } else {
      var img = document.createElement('img');
      img.src = shot.url;
      img.alt = '拍摄的照片';
      el.lbStage.appendChild(img);
    }
    el.lbMeta.textContent = (shot.kind === 'video' ? '录像' : '照片') + ' · ' +
      shot.width + '×' + shot.height + ' · ' + fmtBytes(shot.blob.size) + ' · ' + shot.name;
    el.lbDownload.href = shot.url;
    el.lbDownload.download = shot.name;
    clearTimeout(toastTimer);
    el.toast.classList.remove('is-on');
    el.lightbox.hidden = false;
  }

  function closeLightbox() {
    el.lightbox.hidden = true;
    el.lbStage.innerHTML = '';
    state.activeShot = null;
  }

  function deleteActiveShot() {
    var shot = state.activeShot;
    if (!shot) return;
    var i = state.shots.indexOf(shot);
    if (i >= 0) state.shots.splice(i, 1);
    URL.revokeObjectURL(shot.url);
    closeLightbox();
    renderShots();
    toast('已删除');
  }

  function clearAll() {
    if (!state.shots.length) return;
    state.shots.forEach(function (shot) { URL.revokeObjectURL(shot.url); });
    state.shots = [];
    renderShots();
    toast('已清空拍摄记录');
  }

  /* ---------------- controls ---------------- */
  function switchCamera() {
    if (state.recording) stopRecording();
    return listCameras().then(function (devices) {
      if (devices.length < 2) { toast('只检测到一个摄像头', true); return; }
      state.deviceIndex = (state.deviceIndex + 1) % devices.length;
      var next = devices[state.deviceIndex];
      return startCamera(next.deviceId).then(function () {
        var label = (next.label || '').toLowerCase();
        if (label) {
          var front = /front|user|facetime|内建|前置/.test(label);
          state.facing = front ? 'user' : 'environment';
          el.hudFacing.textContent = front ? '前置' : '后置';
        }
      });
    });
  }

  function toggleMirror() {
    state.mirror = !state.mirror;
    el.stage.classList.toggle('mirrored', state.mirror);
    el.mirrorBtn.setAttribute('aria-pressed', String(state.mirror));
  }

  function toggleGrid() {
    state.grid = !state.grid;
    el.grid.classList.toggle('is-on', state.grid);
    el.gridBtn.setAttribute('aria-pressed', String(state.grid));
  }

  function toggleTorch() {
    var track = state.stream && state.stream.getVideoTracks()[0];
    if (!track || !track.getCapabilities || !track.getCapabilities().torch) return Promise.resolve();
    state.torchOn = !state.torchOn;
    return track.applyConstraints({ advanced: [{ torch: state.torchOn }] }).then(function () {
      el.torchBtn.setAttribute('aria-pressed', String(state.torchOn));
    }).catch(function () {
      state.torchOn = false;
      toast('该设备不支持补光灯', true);
    });
  }

  function toggleFullscreen() {
    if (!document.fullscreenElement) {
      return el.stage.requestFullscreen().then(function () {
        if (screen.orientation && screen.orientation.lock) screen.orientation.lock('portrait').catch(function () {});
      }).catch(function (err) {
        toast('无法进入全屏：' + err.message, true);
      });
    }
    return document.exitFullscreen().catch(function () {});
  }
  document.addEventListener('fullscreenchange', function () {
    el.fullBtn.setAttribute('aria-pressed', String(!!document.fullscreenElement));
  });

  /* ---------------- 手机推流（WebRTC 点对点） ----------------
     手机采集 → 电脑显示；音视频点对点直连，不经过服务器，服务器只转发 SDP/ICE 信令。 */
  var rtc = {
    room: '', role: '', pc: null, seq: 0, timer: null, retry: null,
    pending: [], dismissed: false, retries: 0, sawPeer: false, phoneRoom: '', gone: false
  };

  function randomRoom() {
    var buf = new Uint8Array(6);
    if (window.crypto && crypto.getRandomValues) crypto.getRandomValues(buf);
    else for (var i = 0; i < buf.length; i++) buf[i] = Math.floor(Math.random() * 256);
    return Array.prototype.map.call(buf, function (b) { return ('0' + b.toString(36)).slice(-2); }).join('');
  }

  function rtcSend(kind, data) {
    if (!rtc.room) return Promise.resolve(null);
    return fetch('/api/rtc/send', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ room: rtc.room, role: rtc.role, kind: kind, data: data })
    }).then(function (r) { return r.ok ? r.json() : null; }).catch(function () { return null; });
  }

  /* 把游标推到房间消息末尾，丢掉上一次会话遗留的 offer/answer/bye —— 否则重推流会重放旧信令把自己关掉 */
  function syncCursor() {
    if (!rtc.room) return Promise.resolve();
    var url = '/api/rtc/poll?room=' + encodeURIComponent(rtc.room) +
      '&role=' + rtc.role + '&since=0&wait=0';
    return fetch(url, { cache: 'no-store' })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) { if (data && data.ok) rtc.seq = data.cursor || 0; })
      .catch(function () {});
  }

  function rtcPollOnce() {
    if (!rtc.room) return Promise.resolve();
    var url = '/api/rtc/poll?room=' + encodeURIComponent(rtc.room) +
      '&role=' + rtc.role + '&since=' + rtc.seq + '&wait=8';
    return fetch(url, { cache: 'no-store' })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (!data || !data.ok) return;
        rtc.seq = data.cursor || rtc.seq;
        (data.msgs || []).forEach(handleSignal);
        if (rtc.role === 'viewer' && !state.remote) {
          var online = (data.peers || []).indexOf('phone') !== -1;
          if (rtc.gone) {
            /* 对方刚发过 bye，peers 里还会残留十几秒，别显示成"正在连接" */
            setWaitStatus('手机已断开，等待重新接入…');
          } else if (online) {
            rtc.sawPeer = true;
            setWaitStatus('手机已接入，正在建立连接…', true);
          } else {
            setWaitStatus(rtc.sawPeer ? '手机已断开，等待重新接入…' : '等待手机接入…');
          }
        }
      })
      .catch(function () { /* 网络抖动，下一轮继续 */ });
  }

  function loopPoll() {
    if (!rtc.room) return;
    rtcPollOnce().then(function () {
      if (!rtc.room) return;
      rtc.timer = setTimeout(loopPoll, 0);
    });
  }

  function handleSignal(msg) {
    if (!msg || !msg.kind) return;
    if (msg.kind === 'offer') acceptOffer(msg.data);
    else if (msg.kind === 'answer') acceptAnswer(msg.data);
    else if (msg.kind === 'ice') acceptIce(msg.data);
    else if (msg.kind === 'bye') onPeerBye();
  }

  function newPeer() {
    var pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] });
    pc.onicecandidate = function (e) {
      if (e.candidate) rtcSend('ice', e.candidate.toJSON ? e.candidate.toJSON() : e.candidate);
    };
    return pc;
  }

  function flushIce() {
    var queue = rtc.pending;
    rtc.pending = [];
    queue.forEach(function (c) { acceptIce(c); });
  }

  /* ----- 电脑端：生成房间、等手机接入、显示画面 ----- */
  function setWaitStatus(text, on) {
    if (!el.waitStatus) return;
    el.waitStatus.textContent = text;
    el.waitStatus.classList.toggle('is-on', !!on);
  }

  var LAN_HINT_NONE = '还没拿到局域网地址。请确认这台电脑已连上 Wi-Fi / 网线，稍等几秒会自动更新。';

  function setWaitHint() {
    if (!el.waitHint) return;
    var bad = false;
    if (!usableBase(lanBase)) {
      el.waitHint.textContent = location.protocol === 'file:'
        ? '直接双击打开时拿不到局域网地址 —— 请运行 python3 serve.py 后访问 localhost。'
        : LAN_HINT_NONE;
      bad = true;
    } else {
      bad = !!lanCert && /^https:/.test(lanBase) &&
        lanCert.indexOf(new URL(lanBase).hostname) === -1;
      el.waitHint.textContent = bad ? LAN_HINT_MISMATCH : LAN_HINT;
    }
    el.waitHint.classList.toggle('is-warn', bad);
  }

  function renderWaitQR(url) {
    if (!url) {
      /* 还没拿到手机能连的地址：先不画 localhost 的二维码 */
      el.waitQr.hidden = true;
      el.waitUrl.textContent = '正在获取局域网地址…';
      setWaitHint();
      return;
    }
    if (!window.qrcode) return;
    try {
      var qr = window.qrcode(0, 'M');
      qr.addData(url);
      qr.make();
      el.waitQr.innerHTML = qr.createSvgTag({ cellSize: 4, margin: 0, scalable: true });
      el.waitUrl.textContent = url;
      el.waitQr.hidden = false;
    } catch (err) {
      el.waitQr.hidden = true;
    }
  }

  /* 手机能不能连上这个地址：localhost / 127.x 在手机上指向手机自己，没用 */
  function usableBase(base) {
    if (!base) return false;
    try {
      var u = new URL(base);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
      return !/^(localhost|127\.\d+\.\d+\.\d+|\[::1\]|::1)$/.test(u.hostname);
    } catch (err) { return false; }
  }

  /* 二维码里的地址必须是手机能连上的：打开页面用的是 localhost 时，
     换成服务端探测到的局域网 IP，否则手机扫到 localhost 会指向它自己。 */
  function castUrl(room) {
    var base = lanBase || (location.origin + location.pathname);
    if (!usableBase(base)) return '';
    return base + (base.indexOf('?') === -1 ? '?' : '&') + 'rtc=' + room;
  }

  function startViewer() {
    if (rtc.room && rtc.role === 'viewer') return;
    if (rtc.role === 'phone') stopPhoneCast(true);
    rtc.role = 'viewer';
    rtc.room = randomRoom();
    rtc.seq = 0;
    rtc.sawPeer = false;
    rtc.gone = false;
    el.stage.classList.add('is-waiting');
    el.stageWait.hidden = false;
    el.rtcWait.hidden = false;
    el.phoneCastBtn.hidden = true;
    setWaitHint();
    renderWaitQR(castUrl(rtc.room));
    setWaitStatus('等待手机接入…');
    // 还没拿到局域网地址时补一次，拿到后二维码会自动换成手机能连的地址
    if (!lanBase) refreshShare();
    rtcSend('open', null);
    loopPoll();
  }

  /* 只断开当前画面，房间保留 —— 手机重新推流就能接回来 */
  function disconnectRemote() {
    rtcSend('bye', null);
    endRemote();
    el.stage.classList.add('is-waiting');
    el.stageWait.hidden = false;
    el.rtcWait.hidden = false;
    setWaitStatus('已断开，等待手机重新接入…');
    el.rtcStatus.textContent = '已断开，等待手机重新接入…';
    setStatus('idle', '等待手机');
    el.hudFacing.textContent = '—';
    el.fpsPill.textContent = '— FPS';
  }

  function stopViewer() {
    rtc.room = '';
    rtc.role = '';
    clearTimeout(rtc.timer);
    rtc.timer = null;
    el.stage.classList.remove('is-waiting');
    el.stageWait.hidden = true;
    el.rtcWait.hidden = true;
    el.phoneCastBtn.hidden = false;
    endRemote(false);
  }

  function acceptOffer(sdp) {
    if (!sdp || rtc.role !== 'viewer') return;
    rtc.gone = false;
    if (rtc.pc) { try { rtc.pc.close(); } catch (e) {} rtc.pc = null; }
    var pc = newPeer();
    rtc.pc = pc;
    pc.ontrack = function (e) {
      var stream = (e.streams && e.streams[0]) || state.remoteStream;
      if (!stream) return;
      state.remoteStream = stream;
      state.remote = true;
      el.remoteVideo.srcObject = stream;
      el.remoteVideo.hidden = false;
      el.remoteVideo.play().catch(function () {});
      el.stage.classList.remove('is-waiting');
      el.stageWait.hidden = true;
      el.stage.classList.add('is-remote');
      el.stage.classList.remove('mirrored');
      el.captureBtn.disabled = false;
      el.recordBtn.disabled = false;
      el.gridBtn.disabled = false;
      el.fullBtn.disabled = false;
      el.shutterLabel.textContent = '手机画面';
      setStatus('live', '手机画面');
      el.hudFacing.textContent = '手机';
      el.rtcStatus.textContent = '已连接，画面来自手机';
      if (el.rtcNote) el.rtcNote.textContent = '这台电脑正在显示手机镜头。点「断开画面」可暂停，手机随时能重新推流。';
      el.rtcCopy.hidden = true;
      el.rtcCancel.textContent = '断开画面';
      keepAwake();
      updateResolution();
      startFpsMeter();
      sampleQuality();
      toast('手机画面已接入');
    };
    pc.onconnectionstatechange = function () {
      if (pc.connectionState === 'connected') setWaitStatus('已连接', true);
      else if (pc.connectionState === 'disconnected') setStatus('idle', '连接中断');
    };
    pc.setRemoteDescription(new RTCSessionDescription(sdp))
      .then(function () { return pc.createAnswer(); })
      .then(function (answer) { return pc.setLocalDescription(answer); })
      .then(function () {
        flushIce();
        return rtcSend('answer', { type: pc.localDescription.type, sdp: pc.localDescription.sdp });
      })
      .catch(function (err) { console.error(err); toast('建立连接失败：' + err.message, true); });
  }

  function acceptAnswer(sdp) {
    if (!sdp || !rtc.pc || rtc.pc.signalingState === 'stable') return;
    rtc.pc.setRemoteDescription(new RTCSessionDescription(sdp))
      .then(flushIce)
      .catch(function (err) { console.error(err); });
  }

  function acceptIce(candidate) {
    if (!candidate) return;
    if (!rtc.pc || !rtc.pc.remoteDescription) { rtc.pending.push(candidate); return; }
    rtc.pc.addIceCandidate(new RTCIceCandidate(candidate)).catch(function () {});
  }

  function onPeerBye() {
    if (rtc.role === 'viewer') {
      rtc.gone = true;
      endRemote(false);
      el.stage.classList.add('is-waiting');
      el.stageWait.hidden = false;
      el.rtcWait.hidden = false;
      setWaitStatus('手机已断开，等待重新接入…');
      el.rtcStatus.textContent = '手机已断开，等待重新接入…';
      setStatus('idle', '等待手机');
      el.shutterLabel.textContent = '启用摄像头';
      el.hudFacing.textContent = '—';
      el.fpsPill.textContent = '— FPS';
    } else if (rtc.role === 'phone') {
      stopPhoneCast(false);
      el.phoneCastLabel.textContent = '重新推流到电脑';
      toast('电脑端已停止接收', true);
    }
  }

  function endRemote() {
    if (rtc.pc) { try { rtc.pc.close(); } catch (e) {} rtc.pc = null; }
    rtc.pending = [];
    state.remote = false;
    state.remoteStream = null;
    el.remoteVideo.srcObject = null;
    el.remoteVideo.hidden = true;
    el.stage.classList.remove('is-remote');
    if (el.rtcCopy) el.rtcCopy.hidden = false;
    if (el.rtcNote) el.rtcNote.textContent = '点上方按钮生成链接，手机扫码打开后会自动接入，这台电脑直接显示手机摄像头的实时画面。';
    if (el.rtcCancel) el.rtcCancel.textContent = '取消';
    if (el.rtcStatus) el.rtcStatus.textContent = '等待手机接入…';
    clearTimeout(qualityTimer);
    qualityTimer = null;
    setQuality('');
    el.shutterLabel.textContent = '启用摄像头';
    el.fpsPill.textContent = '— FPS';
    if (state.started) {
      setStatus(state.recording ? 'rec' : 'live', state.recording ? '录制中' : '实时画面');
    } else {
      setStatus('idle', '待机');
    }
  }

  /* ----- 手机端：把本机摄像头推给电脑 ----- */
  function startPhoneCast(room) {
    if (rtc.room === room && rtc.role === 'phone' && rtc.pc) return Promise.resolve();
    rtc.room = room;
    rtc.role = 'phone';
    rtc.retries = 0;
    el.castBar.hidden = false;
    document.body.classList.add('is-casting');
    el.idle.classList.remove('is-invite');
    el.phoneCastBtn.hidden = true;
    el.phoneCastLabel.textContent = '用手机当摄像头';
    el.rtcStatus.textContent = '正在连接电脑…';
    if (el.rtcNote) el.rtcNote.textContent = '正在把画面推送到电脑。手机切到后台或锁屏会中断推流，回来点「重新推流到电脑」即可。';
    el.rtcWait.hidden = false;
    el.rtcCopy.hidden = true;
    el.rtcCancel.hidden = true;
    el.castText.textContent = '正在打开摄像头…';
    el.castInvite.hidden = true;
    el.castStartBtn.hidden = true;
    el.castDismiss.hidden = true;
    el.localCamBtn.hidden = true;

    return startCamera().then(function () {
      if (!state.stream) throw new Error('无法打开摄像头');
      var pc = newPeer();
      rtc.pc = pc;
      var senders = state.stream.getTracks().map(function (t) {
        // detail：编码器优先保清晰度而不是优先保帧率
        try { t.contentHint = 'detail'; } catch (e) {}
        return pc.addTrack(t, state.stream);
      });
      tuneSenders(senders);
      pc.onconnectionstatechange = function () {
        if (pc.connectionState === 'connected') {
          el.castText.textContent = '正在推流到电脑';
          el.rtcStatus.textContent = '正在推流到电脑';
          clearTimeout(rtc.retry);
          toast('已连接到电脑');
        } else if (pc.connectionState === 'failed') {
          el.castText.textContent = '连接中断，正在重试…';
          el.rtcStatus.textContent = '连接中断，正在重试…';
          scheduleRetry();
        }
      };
      return syncCursor()
        .then(function () { return pc.createOffer(); })
        .then(function (offer) { return pc.setLocalDescription(offer); })
        .then(function () {
          rtcSend('offer', { type: pc.localDescription.type, sdp: pc.localDescription.sdp });
          el.castText.textContent = '等待电脑接收…';
          loopPoll();
          scheduleRetry();
        });
    }).catch(function (err) {
      console.error(err);
      toast(describeError(err), true);
      stopPhoneCast(true);
    });
  }

  /* 默认的 WebRTC 编码在带宽波动时会先砍分辨率（画面立刻变糊），
     这里显式要求「保分辨率」，并把码率上限抬高，局域网内能跑满。 */
  function tuneSenders(senders) {
    senders.forEach(function (sender) {
      if (!sender || !sender.getParameters) return;
      var params = sender.getParameters();
      if (!params.encodings || !params.encodings.length) params.encodings = [{}];
      params.encodings[0].maxBitrate = 12000000;
      params.encodings[0].maxFramerate = 30;
      params.encodings[0].scaleResolutionDownBy = 1;
      params.degradationPreference = 'maintain-resolution';
      sender.setParameters(params).catch(function (err) { console.warn('setParameters failed', err); });
    });
  }

  /* 电脑端可能晚于手机打开，定期重发 offer，直到接通 */
  function scheduleRetry() {
    clearTimeout(rtc.retry);
    rtc.retry = setTimeout(function () {
      if (rtc.role !== 'phone' || !rtc.pc || !rtc.pc.localDescription) return;
      if (rtc.pc.connectionState === 'connected') return;
      rtc.retries++;
      rtcSend('offer', { type: rtc.pc.localDescription.type, sdp: rtc.pc.localDescription.sdp });
      if (rtc.retries > 40) { toast('电脑端没有响应，请确认它已打开页面', true); return; }
      scheduleRetry();
    }, 12000);
  }

  function stopPhoneCast(notify) {
    clearTimeout(rtc.retry);
    clearTimeout(rtc.timer);
    rtc.timer = null;
    if (notify) rtcSend('bye', null);
    if (rtc.pc) { try { rtc.pc.close(); } catch (e) {} rtc.pc = null; }
    rtc.room = '';
    rtc.role = '';
    el.castBar.hidden = true;
    document.body.classList.remove('is-casting');
    el.phoneCastBtn.hidden = false;
    el.rtcWait.hidden = true;
    el.rtcCopy.hidden = false;
    el.rtcCancel.hidden = false;
    el.rtcStatus.textContent = '等待手机接入…';
    if (el.rtcNote) el.rtcNote.textContent = '点上方按钮生成链接，手机扫码打开后会自动接入，这台电脑直接显示手机摄像头的实时画面。';
    if (state.recording) stopRecording();
  }

  /* ----- 入口 ----- */
  function setupCast() {
    var room = new URLSearchParams(location.search).get('rtc');
    if (!room) return;
    rtc.room = room;
    rtc.phoneRoom = room;
    rtc.role = 'phone';
    rtc.dismissed = false;
    el.castInvite.hidden = false;
    el.castStartBtn.hidden = false;
    el.castDismiss.hidden = false;
    el.localCamBtn.hidden = false;
    el.idle.classList.add('is-invite');
    el.hint.textContent = '这台手机将作为电脑的摄像头';
    setStatus('idle', '等待推流');
    el.startBtn.addEventListener('click', function (e) {
      if (rtc.dismissed || rtc.role !== 'phone' || !rtc.room) return;
      e.stopImmediatePropagation();
      e.preventDefault();
      startPhoneCast(rtc.room);
    }, true);
    el.castStartBtn.addEventListener('click', function () { startPhoneCast(rtc.room); });
    el.castDismiss.addEventListener('click', function () {
      rtc.dismissed = true;
      rtc.room = '';
      rtc.role = '';
      el.castInvite.hidden = true;
      el.castStartBtn.hidden = true;
      el.castDismiss.hidden = true;
      el.localCamBtn.hidden = true;
      el.hint.textContent = '首次使用会弹出权限请求，请选择「允许」';
      setStatus('idle', '待机');
    });
    el.localCamBtn.addEventListener('click', function () {
      rtc.dismissed = true;
      el.castInvite.hidden = true;
      el.castStartBtn.hidden = true;
      el.castDismiss.hidden = true;
      el.localCamBtn.hidden = true;
      startCamera();
    });
  }

  /* ---------------- QR / share ---------------- */
  function renderQR(url) {
    if (!url || !window.qrcode) return false;
    try {
      var qr = window.qrcode(0, 'M');
      qr.addData(url);
      qr.make();
      el.qrBox.innerHTML = qr.createSvgTag({ cellSize: 4, margin: 0, scalable: true });
      el.qrUrl.textContent = url;
      return true;
    } catch (err) {
      console.warn('QR render failed', err);
      return false;
    }
  }

  /* 地址不是写死的：每次都用「当前实际在用的域名/IP」去问服务端要二维码地址，
     换 Wi-Fi、换了端口、挂到公网域名下都会自动跟着变。 */
  var shareTimer = null;
  var lastShare = '';
  var certIps = null;
  /* 手机能访问的基地址（永远是局域网 IP，不会是 localhost）：配对二维码用它 */
  var lanBase = '';
  var lanCert = null;
  var LAN_HINT = '手机与电脑需在同一个 Wi-Fi；首次访问需点「继续访问」信任本地证书。';
  var LAN_HINT_MISMATCH = '服务正在为新地址重签证书，十几秒后会自动更新；期间扫码可能提示「不安全」。';
  var NOTE_HTML = '手机与电脑连同一个 Wi-Fi，扫码即可。<br>地址会自动检测当前网络，换 Wi-Fi 后刷新页面即可更新。<br>首次访问需在提示中「继续访问」以信任本地证书。';

  function setShareUrl(url) {
    if (!url) return;
    var host = '';
    try { host = new URL(url).hostname; } catch (err) { host = ''; }
    // 配对二维码单独用局域网地址；地址变了就重画（等待手机期间才需要）
    if (url !== lanBase) {
      lanBase = url;
      if (rtc.role === 'viewer' && rtc.room) {
        renderWaitQR(castUrl(rtc.room));
        setWaitHint();
      }
    }
    if (url !== lastShare) {
      if (!renderQR(url)) return;
      lastShare = url;
      el.qrPanel.hidden = false;
      if (el.metaHost) el.metaHost.textContent = host || '—';
    }
    // 地址是自动取的，万一取到的地址不在证书里，提醒一下
    if (el.qrNote) {
      var mismatch = certIps && /^\d+\.\d+\.\d+\.\d+$/.test(host) && certIps.indexOf(host) === -1;
      el.qrNote.innerHTML = mismatch
        ? '当前地址 ' + host + ' 不在本地证书中，手机可能提示「不安全」。<br>服务会自动为新地址重签证书，等十几秒刷新即可。'
        : NOTE_HTML;
    }
  }

  function refreshShare() {
    var override = new URLSearchParams(location.search).get('url');
    if (override) { setShareUrl(override); return; }
    if (location.protocol === 'file:') { el.qrPanel.hidden = true; return; }
    fetch('/api/info', { cache: 'no-store' })
      .then(function (res) { return res.ok ? res.json() : null; })
      .then(function (info) {
        if (info && (info.lanShareUrl || info.shareUrl)) {
          certIps = Array.isArray(info.certIps) ? info.certIps : null;
          lanCert = certIps;
          setShareUrl(info.lanShareUrl || info.shareUrl);
          return;
        }
        // 没有本地服务（比如传到静态托管）：直接用当前访问的域名
        var host = location.hostname;
        var isLocal = host === 'localhost' || host === '127.0.0.1' || host === '[::1]';
        if (!isLocal) setShareUrl(location.origin + location.pathname);
      })
      .catch(function () { el.qrPanel.hidden = true; });
  }

  function setupShare() {
    // 手机自己打开时二维码没有意义（是给电脑扫码用的）
    if (/Mobi|Android|iPhone|iPad|iPod/i.test(navigator.userAgent) && window.matchMedia('(max-width: 900px)').matches) {
      el.qrPanel.hidden = true;
      return;
    }
    refreshShare();
    // 回到页面、网络恢复、地址变化时都重新取一次，避免显示过期地址
    document.addEventListener('visibilitychange', function () {
      if (document.visibilityState === 'visible') refreshShare();
    });
    window.addEventListener('online', refreshShare);
    window.addEventListener('offline', function () { /* 断网时保留上一次的地址 */ });
    if (shareTimer) clearInterval(shareTimer);
    shareTimer = setInterval(function () {
      if (document.visibilityState === 'visible') refreshShare();
    }, 10000);
  }

  /* ---------------- wiring ---------------- */
  el.startBtn.addEventListener('click', function () { startCamera(); });
  el.captureBtn.addEventListener('click', capture);
  el.recordBtn.addEventListener('click', toggleRecording);
  el.switchBtn.addEventListener('click', switchCamera);
  el.mirrorBtn.addEventListener('click', toggleMirror);
  el.gridBtn.addEventListener('click', toggleGrid);
  el.torchBtn.addEventListener('click', toggleTorch);
  el.fullBtn.addEventListener('click', toggleFullscreen);
  el.clearBtn.addEventListener('click', clearAll);
  el.lbClose.addEventListener('click', closeLightbox);
  el.lbDelete.addEventListener('click', deleteActiveShot);
  el.lightbox.addEventListener('click', function (e) { if (e.target === el.lightbox) closeLightbox(); });

  /* 手机推流相关按钮 */
  if (el.phoneCastBtn) el.phoneCastBtn.addEventListener('click', function () {
    if (rtc.phoneRoom) startPhoneCast(rtc.phoneRoom);
    else startViewer();
  });
  if (el.rtcCancel) el.rtcCancel.addEventListener('click', function () {
    if (state.remote) { disconnectRemote(); toast('已断开手机画面，可重新扫码接入'); }
    else { stopViewer(); toast('已取消，等待手机接入已停止'); }
  });
  if (el.rtcCopy) el.rtcCopy.addEventListener('click', function () {
    if (!rtc.room) return;
    var url = castUrl(rtc.room);
    if (!url) { toast('还没拿到局域网地址，稍等一下再复制'); return; }
    var done = function () { toast('链接已复制，发到手机上打开即可'); };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(url).then(done).catch(function () { window.prompt('复制这个链接发给手机：', url); });
    } else {
      window.prompt('复制这个链接发给手机：', url);
    }
  });
  if (el.castStop) el.castStop.addEventListener('click', function () {
    stopPhoneCast(true);
    toast('已停止推流');
  });

  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && !el.lightbox.hidden) { closeLightbox(); return; }
    var tag = (e.target && e.target.tagName) || '';
    if (/INPUT|TEXTAREA|SELECT/.test(tag)) return;
    if (e.code === 'Space') {
      e.preventDefault();
      if (!el.captureBtn.disabled) capture();
    } else if (e.key === 'r' || e.key === 'R') {
      if (!el.recordBtn.disabled) toggleRecording();
    } else if (e.key === 'f' || e.key === 'F') {
      if (!el.fullBtn.disabled) toggleFullscreen();
    } else if (e.key === 'm' || e.key === 'M') {
      if (!el.mirrorBtn.disabled) toggleMirror();
    }
  });

  window.addEventListener('pagehide', function () {
    releaseWake();
    if (state.recording) stopRecording();
    if (state.stream) state.stream.getTracks().forEach(function (t) { t.stop(); });
  });

  /* ---------------- boot ---------------- */
  var isFile = location.protocol === 'file:';
  el.metaSecure.textContent = isFile ? '本地文件' : (window.isSecureContext ? 'HTTPS 安全' : 'HTTP 不安全');
  if (isFile) {
    el.hint.textContent = '直接双击打开时浏览器会拦截摄像头 — 请运行 python3 serve.py 后访问 localhost';
    el.hint.classList.add('is-warn');
    setStatus('error', '需通过服务器打开');
  } else if (!isSecure) {
    el.hint.textContent = '需要 HTTPS 或 localhost 才能访问摄像头 — 请用随附的本地服务器打开';
    el.hint.classList.add('is-warn');
    setStatus('error', '不安全来源');
  } else {
    el.hint.textContent = '首次使用会弹出权限请求，请选择「允许」';
  }

  if (navigator.permissions && navigator.permissions.query) {
    navigator.permissions.query({ name: 'camera' }).then(function (p) {
      if (p.state === 'denied') {
        setStatus('error', '权限被拒绝');
        el.hint.textContent = '摄像头权限已被拒绝，请在浏览器设置中允许后刷新页面';
        el.hint.classList.add('is-warn');
      } else if (p.state === 'granted' && isSecure) {
        el.hint.textContent = '权限已授予，点击上方按钮即可开始';
      }
      p.onchange = function () {
        if (p.state === 'granted' && !state.started) setStatus('idle', '待机');
      };
    }).catch(function () {});
  }

  renderShots();
  setupShare();
  setupCast();

  window.__MONITOR__ = { state: state, rtc: rtc, capture: capture, startCamera: startCamera, stopRecording: stopRecording, toggleRecording: toggleRecording };
})();