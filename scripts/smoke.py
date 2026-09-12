#!/usr/bin/env python3
"""Real local MariaDB + fault-injecting OpenAI mock. Never touches non-app databases."""
import os, json, time, threading, subprocess, tempfile, pathlib, shutil, urllib.request, urllib.error, http.cookiejar, http.server, socket, socketserver, select
ROOT=pathlib.Path(__file__).resolve().parents[1]
BINARY=ROOT/('target/debug/doccraft-agent.exe' if os.name=='nt' else 'target/debug/doccraft-agent')
PORT=18765
MOCK=18767
requests=[]
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self,*args): pass
    def do_POST(self):
        payload=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        requests.append(payload)
        if self.path.endswith('/count'):
            return self.reply({'input_tokens':len(json.dumps(payload).encode())+256})
        model=payload['model']
        if model=='slow': time.sleep(15)
        if model in ['restart','conflict','db-outage'] and not getattr(self.server,model+'_delayed',False):
            setattr(self.server,model+'_delayed',True);time.sleep(3)
        content=payload['messages'][-1]['content']
        if content=='Reply with OK only.': result='OK'
        else:
            data=json.loads(content)
            instruction=data.get('instruction','')
            if model=='context-once' and not getattr(self.server,'context_sent',False):
                self.server.context_sent=True
                return self.reply({'error':{'code':'context_length_exceeded'}},400)
            if model=='retry-once' and not getattr(self.server,'retry_sent',False):
                self.server.retry_sent=True
                return self.reply({'error':{'code':'rate_limit'}},429)
            if model=='quota-partial' and 'Write only Markdown' in instruction:
                writes=sum(1 for r in requests if r['model']==model and 'Write only Markdown' in r['messages'][-1]['content'])
                if writes>1: return self.reply({'error':{'code':429,'message':'Rate limit exceeded: free-models-per-day'}},429)
            if 'sections:[{title' in instruction:
                result=json.dumps({'sections':[{'title':'기능 개요','query':'handle_request handleRequest validation'},{'title':'오류와 제약','query':'name_required errors'},{'title':'처리 흐름','query':'return handle_request'}]},ensure_ascii=False)
            elif 'Review this section' in instruction:
                if model=='review-once' and not getattr(self.server,'review_sent',False):
                    self.server.review_sent=True
                    result=json.dumps({'issues':[{'severity':'major','section':data['section_index'],'message':'Document the missing empty-name condition','query':'name_required'}]})
                else:result=json.dumps({'issues':[]})
            else:
                evidences=data.get('evidence',[])
                eid=evidences[0]['id'] if evidences else 0
                result=f'입력 이름을 확인하고 처리 결과를 반환합니다. [E:{eid}]\n\n```mermaid\nflowchart LR\n A["입력"] --> B["이름 검증"]\n B --> C["결과 반환"]\n```'
                if model=='headings':result='## '+data['title']+'\n\n## Details\n'+result
                if data.get('correction'):result+=f'\n\n빈 이름의 오류 조건을 추가로 정리했습니다. [E:{eid}]'
                if model=='mermaid-once' and not getattr(self.server,'mermaid_sent',False):
                    self.server.mermaid_sent=True;result=f'Keep this fact [E:{eid}]\n\n```mermaid\nflowchart LR\n A[broken\n```'
                if model=='invalid-once' and not getattr(self.server,'invalid_sent',False):
                    self.server.invalid_sent=True
                    result='Unsupported claim [E:999999999]'
        finish='stop'
        if model=='truncate-once' and content!='Reply with OK only.' and 'Write only Markdown' in instruction and not getattr(self.server,'truncated',False):
            self.server.truncated=True;finish='length'
        body={'choices':[{'message':{'content':result},'finish_reason':finish}],'usage':{'prompt_tokens':100,'completion_tokens':100,'completion_tokens_details':{'reasoning_tokens':0}}}
        self.reply(body)
    def reply(self,body,status=200):
        encoded=json.dumps(body).encode()
        try:
            self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(encoded)));self.end_headers();self.wfile.write(encoded)
        except (BrokenPipeError,ConnectionResetError): pass


class DbProxy(socketserver.ThreadingTCPServer):
    allow_reuse_address=True
    daemon_threads=True
    def __init__(self,address):
        self.enabled=True;self.peers=[];self.lock=threading.Lock()
        super().__init__(address,DbForward)
    def disconnect(self):
        self.enabled=False
        with self.lock:
            for peer in self.peers:
                try:peer.shutdown(socket.SHUT_RDWR)
                except OSError:pass
class DbForward(socketserver.BaseRequestHandler):
    def handle(self):
        if not self.server.enabled:return
        upstream=socket.create_connection(('127.0.0.1',3306),timeout=5)
        peers=[self.request,upstream]
        with self.server.lock:self.server.peers.extend(peers)
        try:
            while self.server.enabled:
                ready,_,_=select.select(peers,[],[],.2)
                for src in ready:
                    data=src.recv(65536)
                    if not data:return
                    (upstream if src is self.request else self.request).sendall(data)
        except OSError:pass
        finally:
            with self.server.lock:
                for peer in peers:
                    if peer in self.server.peers:self.server.peers.remove(peer)
            upstream.close()

def main():
    password=os.environ.get('DOCCRAFT_DB_PASSWORD')
    if password is None: raise SystemExit('Set DOCCRAFT_DB_PASSWORD for local test database')
    mock=http.server.ThreadingHTTPServer(('127.0.0.1',MOCK),Handler)
    threading.Thread(target=mock.serve_forever,daemon=True).start()
    with tempfile.TemporaryDirectory(prefix='doccraft-e2e-') as tmp:
        tmp=pathlib.Path(tmp); source=tmp/'sources';shutil.copytree(ROOT/'tests/fixtures',source);out=tmp/'output';out.mkdir()
        env={**os.environ,'DOCCRAFT_PORT':str(PORT),'DOCCRAFT_DATA_DIR':str(tmp/'state'),'DOCCRAFT_DB_NAME':'doccraft_agent_test','DOCCRAFT_DB_PASSWORD':password}
        log=open(tmp/'server.log','w+')
        process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
        jar=http.cookiejar.CookieJar();opener=urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
        def api(path,method='GET',body=None):
            data=None if body is None else json.dumps(body).encode()
            req=urllib.request.Request(f'http://127.0.0.1:{PORT}/api/v1'+path,data=data,method=method,headers={'Content-Type':'application/json'})
            try:
                with opener.open(req,timeout=15) as response:return json.load(response)
            except urllib.error.HTTPError as e:raise RuntimeError(e.read().decode()) from e
        def poll(run,timeout=60):
            end=time.monotonic()+timeout
            while time.monotonic()<end:
                try:r=next(x for x in api('/runs') if x['id']==run)
                except RuntimeError:time.sleep(.2);continue
                if r['status']=='failed' and (r.get('error') or '').startswith('DB_UNAVAILABLE:'):time.sleep(.2);continue
                if r['status'] not in ['queued','running','cancelling']:return r
                time.sleep(.2)
            raise AssertionError('Run did not finish within deadline')
        def configure(model):
            s=api('/settings');s['llm']['model']=model;s['llm']['base_url']=f'http://127.0.0.1:{MOCK}/v1';s['llm']['rpm']=1000;s['llm']['tpm']=20000000;s['llm']['timeout_seconds']=20;s['llm']['retries']=1;s['source_roots']=[str(source)];s['output_roots']=[str(out)];return api('/settings','PUT',s)
        def task(name):
            return api('/tasks','POST',{'name':name,'sources':[str(source)],'target':str(out/(name+'.md')),'direction':'개발자를 위한 기능 설명과 Mermaid 흐름 정리','max_iterations':2})
        try:
            for _ in range(100):
                try:api('/session');break
                except Exception:
                    if process.poll() is not None:raise AssertionError('Backend exited early')
                    time.sleep(.1)
            else:raise AssertionError('Backend failed to start')
            assert api('/diagnostics')['database']
            configure('normal')
            assert api('/settings/test-db','POST',api('/settings'))['ok']
            assert api('/settings/test-llm','POST',api('/settings'))['ok']
            probe=api('/settings');probe['llm']['reasoning']='on';probe['llm']['effort']='high'
            assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['reasoning_effort']=='high'
            probe['llm']['reasoning']='off';assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['reasoning_effort']=='none'
            probe['llm']['reasoning_parameter']='enable_thinking';assert api('/settings/test-llm','POST',probe)['ok'];assert requests[-1]['chat_template_kwargs']['enable_thinking'] is False
            probe['llm']['proxy_mode']='custom';probe['llm']['proxy_url']=f'http://127.0.0.1:{MOCK}';probe['llm']['base_url']='http://upstream.invalid/v1'
            assert api('/settings/test-llm','POST',probe)['ok']
            print('PASS reasoning mappings and explicit HTTP proxy',flush=True)
            for model in ['normal','context-once','retry-once','invalid-once','review-once','truncate-once','headings','mermaid-once']:
                configure(model);t=task(model);rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
                assert r['status']=='completed',r
                if model=='review-once':assert r['tokens']>=1800,r
                text=(out/(model+'.md')).read_text();assert 'Source references' in text and '```mermaid' in text
                if model in ['invalid-once','truncate-once','mermaid-once']:
                    writes=[json.loads(x['messages'][-1]['content']) for x in requests if x['model']==model and 'Write only Markdown' in x['messages'][-1]['content']]
                    assert writes[0]['evidence']==writes[1]['evidence'], 'Repair discarded evidence'
                    assert len(writes[0]['other_sections'])==2
                    if model=='invalid-once':
                        assert writes[1]['correction']['previous']=='Unsupported claim [E:999999999]'
                        assert 'Maximum 1200 words' in writes[1]['instruction']
                    elif model=='mermaid-once':
                        assert 'Keep this fact' in writes[1]['correction']['previous']
                        assert 'Maximum 1200 words' in writes[1]['instruction']
                    else: assert 'Maximum 600 words' in writes[1]['instruction']
                if model=='headings':
                    assert text.count('## 기능 개요\n')==1 and '### Details' in text
                print('PASS generation / repair:',model,flush=True)
            configure('quota-partial');t=task('quota-partial');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid)
            assert r['status']=='completed_with_warnings',r
            text=(out/'quota-partial.md').read_text()
            assert text.index('부분 생성 · 전체 검토 미완료')<text.index('## 기능 개요')
            assert 'Missing planned sections:' in text
            assert sum(1 for x in requests if x['model']=='quota-partial' and 'Write only Markdown' in x['messages'][-1]['content'])==2
            print('PASS quota: single rejection, partial document banner and missing sections',flush=True)
            configure('slow');t=task('cancel');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            end=time.monotonic()+10
            while time.monotonic()<end and not any(x['model']=='slow' for x in requests):time.sleep(.05)
            start=time.monotonic();assert api(f'/runs/{rid}/cancel','POST')['accepted'];r=poll(rid,3)
            elapsed=time.monotonic()-start
            assert r['status']=='cancelled' and elapsed<2 and not (out/'cancel.md').exists(),r
            print(f'PASS cancellation: {elapsed:.3f}s',flush=True)
            # Source/target equality and forbidden root validation.
            try:api('/tasks','POST',{'name':'bad','sources':['/etc'],'target':str(out/'bad.md'),'direction':'test'})
            except RuntimeError:pass
            else:raise AssertionError('Forbidden root accepted')
            print('PASS allowed-root enforcement',flush=True)
            # A copied task can reuse deterministic source parsing and exact LLM responses.
            configure('normal');t=task('cached');rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid);assert r['status']=='completed',r
            assert r['tokens']==0,r
            assert api('/artifacts')['artifacts']
            print('PASS cache / artifacts / real MariaDB persistence',flush=True)
            configure('conflict');t=task('conflict');(out/'conflict.md').write_text('original')
            rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='conflict' for x in requests):time.sleep(.05)
            (out/'conflict.md').write_text('external edit')
            r=poll(rid);assert r['status']=='completed_with_warnings',r
            assert (out/'conflict.md').read_text()=='external edit'
            assert list(out.glob('conflict.conflict-*.md'))
            print('PASS external edit conflict preservation',flush=True)
            configure('restart');t=task('restart');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='restart' for x in requests):time.sleep(.05)
            process.kill();process.wait();process=subprocess.Popen([str(BINARY)],cwd=ROOT,env=env,stdout=log,stderr=log)
            for _ in range(100):
                try:api('/session');break
                except Exception:time.sleep(.1)
            r=poll(rid);assert r['status']=='completed',r
            print('PASS crash/restart checkpoint recovery',flush=True)
            proxy=DbProxy(('127.0.0.1',18769));threading.Thread(target=proxy.serve_forever,daemon=True).start()
            configure('db-outage');cfg=api('/settings');cfg['db']['port']=18769;api('/settings','PUT',cfg)
            t=task('db-outage');rid=api(f"/tasks/{t['id']}/run",'POST')['id']
            while not any(x['model']=='db-outage' for x in requests):time.sleep(.05)
            proxy.disconnect();time.sleep(6);proxy.enabled=True
            r=poll(rid,45);assert r['status']=='completed',r
            cfg=api('/settings');cfg['db']['port']=3306;api('/settings','PUT',cfg);proxy.shutdown();proxy.server_close()
            print('PASS isolated DB connection outage and automatic recovery',flush=True)
            configure('normal')
            benchmarks=[]
            if os.environ.get('DOCCRAFT_LARGE_TEST')=='1':
                for lines in [100000,1000000]:
                    big=source/('scale-'+str(lines));big.mkdir()
                    for f in range(lines//10000):
                        (big/f'module_{f}.rs').write_text(''.join(f'pub fn f_{f}_{n}() -> i32 {{ {n} }}\n' for n in range(10000)))
                    spec={'name':f'scale-{lines}','sources':[str(big)],'target':str(out/f'scale-{lines}.md'),'direction':'개발자를 위한 기능 설명과 Mermaid 흐름 정리','max_iterations':2}
                    t=api('/tasks','POST',spec);start=time.monotonic();rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid,180);cold=time.monotonic()-start;assert r['status']=='completed',r
                    start=time.monotonic();rid=api(f"/tasks/{t['id']}/run",'POST')['id'];r=poll(rid,180);warm=time.monotonic()-start;assert r['status']=='completed' and r['tokens']==0,r
                    benchmarks.append({'lines':lines,'cold_seconds':round(cold,3),'warm_seconds':round(warm,3),'warm_llm_tokens':r['tokens']})
                    shutil.rmtree(big)
                    print(f'PASS scale {lines}: cold={cold:.2f}s warm={warm:.2f}s',flush=True)
            if os.environ.get('DOCCRAFT_UI_TEST')=='1':
                ui_env={**os.environ,'DOCCRAFT_UI_URL':f'http://127.0.0.1:{PORT}','UI_SOURCE_ROOT':str(source),'UI_OUTPUT_ROOT':str(out)}
                subprocess.run(['node','node_modules/@playwright/test/cli.js','test'],cwd=ROOT/'frontend',env=ui_env,check=True)
            report={'status':'passed','benchmarks':benchmarks,'requests':len(requests),'cancel_seconds':elapsed,'database':'doccraft_agent_test','platform':os.uname().sysname if hasattr(os,'uname') else 'Windows'}
            (ROOT/'docs'/'smoke-results.json').write_text(json.dumps(report,indent=2)+'\n')
        except Exception as error:
            (ROOT/'docs'/'smoke-results.json').write_text(json.dumps({'status':'failed','error':str(error)},indent=2))
            log.flush();log.seek(0);print(log.read());raise
        finally:
            process.terminate()
            try:process.wait(5)
            except subprocess.TimeoutExpired:process.kill();process.wait()
            mock.shutdown();log.close()
if __name__=='__main__':main()
